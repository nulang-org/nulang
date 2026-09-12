//! MPSC mailbox with priority bands and optional capacity limit.
//!
//! Two priority bands (`System` and `Normal`/`Bulk`) ensure that supervisor
//! exit signals and monitor DOWN messages are never delayed behind a queue
//! of regular application messages. When a capacity limit is configured,
//! `System` messages always bypass the limit while `Normal` and `Bulk`
//! messages are rejected with backpressure when the mailbox is full.
//!
//! Concurrent producers use the lock-free `SegQueue`s through `push(&self)`.
//! Scheduler-local traffic uses a plain `VecDeque` through methods requiring
//! `&mut self`. A single atomic logical-message count makes capacity
//! reservation race-safe without exposing scheduler-owned collections through
//! interior mutability.

use crate::vm::Value;
use crossbeam::queue::SegQueue;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Message sent between actors.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub behavior_id: u16,
    /// Payload values, shared via `Arc` to avoid cloning on every
    /// `receive_match` scan. The VM never mutates incoming payloads.
    pub payload: Arc<Vec<Value>>,
    pub sender: u64,
    pub priority: MessagePriority,
    /// W3C traceparent for distributed tracing.
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePriority {
    System = 0,
    Normal = 1,
    Bulk = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiveBuffer {
    System,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReceiveSelection {
    buffer: ReceiveBuffer,
    index: usize,
}

/// MPSC mailbox with priority bands and optional capacity.
///
/// Concurrent producers may call [`Mailbox::push`] through shared references;
/// all operations touching scheduler-local or selective-receive state require
/// `&mut self`. `queued_count` counts logical messages while they move between
/// queues and selective-receive staging buffers.
#[repr(align(64))]
pub struct Mailbox {
    system_queue: SegQueue<Message>,
    normal_queue: SegQueue<Message>,
    /// Same-thread local queue. Only scheduler-owned `&mut self` methods
    /// access it; concurrent producers never touch it.
    local_queue: VecDeque<Message>,
    capacity: usize,
    queued_count: AtomicUsize,
    /// System-priority messages staged during selective receive. Staging is
    /// transactional: candidates remain logically queued until commit.
    system_skip_buffer: VecDeque<(Message, bool)>,
    /// Normal/bulk messages staged during selective receive. The boolean marks
    /// candidates already tried by the current receive expression.
    skip_buffer: VecDeque<(Message, bool)>,
    /// Exact candidate most recently returned to the VM. Earlier rejected
    /// candidates remain marked tried, but commit must consume this selection,
    /// not the first tried candidate.
    receive_selection: Option<ReceiveSelection>,
}

impl Mailbox {
    /// Create a new mailbox.
    ///
    /// `capacity`: maximum total messages allowed. `0` = unbounded.
    /// `System` messages always bypass the limit.
    pub fn new(capacity: usize) -> Self {
        Mailbox {
            system_queue: SegQueue::new(),
            normal_queue: SegQueue::new(),
            local_queue: VecDeque::new(),
            capacity,
            queued_count: AtomicUsize::new(0),
            system_skip_buffer: VecDeque::new(),
            skip_buffer: VecDeque::new(),
            receive_selection: None,
        }
    }

    /// Reserve one logical mailbox slot.
    ///
    /// System messages and unbounded mailboxes always reserve successfully.
    /// Bounded normal/bulk traffic uses CAS so concurrent producers cannot all
    /// observe the same free slot and overfill the mailbox.
    fn reserve_slot(&self, system: bool) -> bool {
        if system || self.capacity == 0 {
            self.queued_count.fetch_add(1, Ordering::AcqRel);
            return true;
        }

        let mut current = self.queued_count.load(Ordering::Acquire);
        loop {
            if current >= self.capacity {
                return false;
            }
            match self.queued_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn release_slot(&self) {
        let previous = self.queued_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "mailbox logical count underflow");
    }

    /// Push a message from a concurrent producer.
    pub fn push(&self, msg: Message) -> Result<(), Message> {
        let system = msg.priority == MessagePriority::System;
        if !self.reserve_slot(system) {
            return Err(msg);
        }
        if system {
            self.system_queue.push(msg);
        } else {
            self.normal_queue.push(msg);
        }
        Ok(())
    }

    /// Push a message from the scheduler thread.
    pub fn push_local(&mut self, msg: Message) -> Result<(), Message> {
        let system = msg.priority == MessagePriority::System;
        if !self.reserve_slot(system) {
            return Err(msg);
        }
        self.local_queue.push_back(msg);
        Ok(())
    }

    /// Pop the highest-priority message.
    pub fn pop(&mut self) -> Option<Message> {
        // Messages already staged by a previous selective receive are older
        // than subsequently-arrived concurrent traffic at the same priority,
        // so consume staged system messages before the live system queue and
        // staged normal messages before the live normal queue.
        let result = self
            .system_skip_buffer
            .pop_front()
            .map(|(m, _)| m)
            .or_else(|| self.system_queue.pop())
            .or_else(|| self.local_queue.pop_front())
            .or_else(|| self.skip_buffer.pop_front().map(|(m, _)| m))
            .or_else(|| self.normal_queue.pop());
        if result.is_some() {
            self.receive_selection = None;
            self.release_slot();
        }
        result
    }

    /// Selective receive: reserve the first untried message whose behavior id
    /// appears in `behavior_ids` without consuming it.
    ///
    /// Every source follows the same transaction lifecycle:
    ///
    /// 1. concurrent/system/local traffic is staged into priority buffers;
    /// 2. the candidate returned to the VM remains logically queued;
    /// 3. a guard rejection simply calls this method again, leaving the prior
    ///    candidate marked tried and selecting the next eligible message;
    /// 4. [`Mailbox::commit_receive_match`] consumes exactly the most recently
    ///    selected candidate;
    /// 5. [`Mailbox::reset_receive_match`] changes no ownership or queue count.
    ///
    /// System-priority candidates are always considered before normal/bulk
    /// candidates. Within each staged buffer FIFO order is preserved.
    pub fn receive_match(&mut self, behavior_ids: &[u16]) -> Option<(usize, Arc<Vec<Value>>)> {
        self.stage_receive_candidates();

        if let Some((index, arm, payload)) =
            Self::scan_staged(&mut self.system_skip_buffer, behavior_ids)
        {
            self.receive_selection = Some(ReceiveSelection {
                buffer: ReceiveBuffer::System,
                index,
            });
            return Some((arm, payload));
        }

        if let Some((index, arm, payload)) = Self::scan_staged(&mut self.skip_buffer, behavior_ids)
        {
            self.receive_selection = Some(ReceiveSelection {
                buffer: ReceiveBuffer::Normal,
                index,
            });
            return Some((arm, payload));
        }

        self.receive_selection = None;
        None
    }

    fn stage_receive_candidates(&mut self) {
        // Preserve same-priority FIFO across retries: previously staged
        // candidates stay at the front; newly-arrived candidates append.
        while let Some(msg) = self.system_queue.pop() {
            self.system_skip_buffer.push_back((msg, false));
        }

        // Scheduler-local traffic is classified into the same two transaction
        // buffers so local candidates no longer have different consume timing.
        while let Some(msg) = self.local_queue.pop_front() {
            if msg.priority == MessagePriority::System {
                self.system_skip_buffer.push_back((msg, false));
            } else {
                self.skip_buffer.push_back((msg, false));
            }
        }

        while let Some(msg) = self.normal_queue.pop() {
            self.skip_buffer.push_back((msg, false));
        }
    }

    fn scan_staged(
        buffer: &mut VecDeque<(Message, bool)>,
        behavior_ids: &[u16],
    ) -> Option<(usize, usize, Arc<Vec<Value>>)> {
        for (index, (msg, tried)) in buffer.iter_mut().enumerate() {
            if *tried {
                continue;
            }
            if let Some(arm) = behavior_ids.iter().position(|&id| id == msg.behavior_id) {
                *tried = true;
                return Some((index, arm, Arc::clone(&msg.payload)));
            }
        }
        None
    }

    /// Total logical message count. Safe to query concurrently.
    pub fn len(&self) -> usize {
        self.queued_count.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain all queues into a cloned snapshot, then restore all messages.
    /// The logical count is intentionally unchanged because this is
    /// observational.
    pub fn drain(&mut self) -> Vec<Message> {
        self.receive_selection = None;
        let mut snapshot = Vec::with_capacity(self.len());
        while let Some((msg, _)) = self.system_skip_buffer.pop_front() {
            snapshot.push(msg);
        }
        while let Some(msg) = self.system_queue.pop() {
            snapshot.push(msg);
        }
        while let Some(msg) = self.local_queue.pop_front() {
            snapshot.push(msg);
        }
        while let Some((msg, _)) = self.skip_buffer.pop_front() {
            snapshot.push(msg);
        }
        while let Some(msg) = self.normal_queue.pop() {
            snapshot.push(msg);
        }
        for msg in &snapshot {
            if msg.priority == MessagePriority::System {
                self.system_queue.push(msg.clone());
            } else {
                self.local_queue.push_back(msg.clone());
            }
        }
        snapshot
    }

    pub fn flush_skip_buffer(&mut self) {
        self.receive_selection = None;
        while let Some((msg, _)) = self.system_skip_buffer.pop_front() {
            self.system_queue.push(msg);
        }
        while let Some((msg, _)) = self.skip_buffer.pop_front() {
            self.normal_queue.push(msg);
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Commit exactly the candidate most recently returned by
    /// [`Mailbox::receive_match`]. The payload is returned to the runtime so it
    /// can establish receiver-side ORCA ownership after physical consumption.
    pub fn commit_receive_match(&mut self) -> Option<Arc<Vec<Value>>> {
        let selection = self.receive_selection.take()?;
        let removed = match selection.buffer {
            ReceiveBuffer::System => self.system_skip_buffer.remove(selection.index),
            ReceiveBuffer::Normal => self.skip_buffer.remove(selection.index),
        };
        let payload = removed.map(|(message, _)| message.payload);
        if payload.is_some() {
            self.release_slot();
        }
        self.clear_receive_attempts();
        payload
    }

    /// Abort the current selective-receive transaction. No message is consumed
    /// and the logical mailbox count is unchanged.
    pub fn reset_receive_match(&mut self) {
        self.receive_selection = None;
        self.clear_receive_attempts();
    }

    fn clear_receive_attempts(&mut self) {
        for (_, tried) in self.system_skip_buffer.iter_mut() {
            *tried = false;
        }
        for (_, tried) in self.skip_buffer.iter_mut() {
            *tried = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(behavior_id: u16, sender: u64) -> Message {
        make_msg_with(behavior_id, sender, 42, MessagePriority::Normal)
    }

    fn make_msg_with(
        behavior_id: u16,
        sender: u64,
        value: i64,
        priority: MessagePriority,
    ) -> Message {
        Message {
            behavior_id,
            payload: Arc::new(vec![Value::int(value)]),
            sender,
            priority,
            trace_id: None,
        }
    }

    #[test]
    fn test_push_and_pop() {
        let mut mb = Mailbox::new(4);
        let msg = make_msg(1, 100);
        assert!(mb.is_empty());
        assert_eq!(mb.len(), 0);
        mb.push(msg.clone()).unwrap();
        assert!(!mb.is_empty());
        assert_eq!(mb.len(), 1);
        let popped = mb.pop().unwrap();
        assert_eq!(popped.behavior_id, 1);
        assert_eq!(popped.sender, 100);
        assert_eq!(*popped.payload, vec![Value::int(42)]);
        assert!(mb.is_empty());
        assert_eq!(mb.pop(), None);
    }

    #[test]
    fn test_unbounded_never_fails() {
        let mut mb = Mailbox::new(0);
        for i in 0..10000 {
            assert!(mb.push(make_msg(i as u16, i as u64)).is_ok());
        }
        assert_eq!(mb.len(), 10000);
        for i in 0..10000 {
            let msg = mb.pop().expect("message should exist");
            assert_eq!(msg.behavior_id, i as u16);
        }
        assert!(mb.is_empty());
    }

    #[test]
    fn bounded_capacity_reservation_is_atomic() {
        use std::thread;

        let mb = Arc::new(Mailbox::new(100));
        let mut handles = Vec::new();
        for t in 0..8 {
            let mb = Arc::clone(&mb);
            handles.push(thread::spawn(move || {
                let mut accepted = 0;
                for i in 0..100 {
                    if mb.push(make_msg(i, t)).is_ok() {
                        accepted += 1;
                    }
                }
                accepted
            }));
        }
        let accepted: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(accepted, 100);
        assert_eq!(mb.len(), 100);
    }

    #[test]
    fn test_supervisor_signals_never_dropped() {
        let mut mb = Mailbox::new(4);
        for i in 0..1000 {
            mb.push(Message {
                behavior_id: 0,
                payload: Arc::new(vec![Value::int(i)]),
                sender: i as u64,
                priority: MessagePriority::System,
                trace_id: None,
            })
            .unwrap();
        }
        assert_eq!(mb.len(), 1000);
        let mut count = 0;
        while mb.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, 1000);
    }

    #[test]
    fn test_len_and_is_empty() {
        let mut mb = Mailbox::new(4);
        assert!(mb.is_empty());
        mb.push(make_msg(10, 1)).unwrap();
        mb.push(make_msg(20, 2)).unwrap();
        mb.push(make_msg(30, 3)).unwrap();
        assert_eq!(mb.len(), 3);
        mb.pop().unwrap();
        assert_eq!(mb.len(), 2);
        mb.pop().unwrap();
        mb.pop().unwrap();
        assert!(mb.is_empty());
    }

    #[test]
    fn test_drain_snapshot() {
        let mut mb = Mailbox::new(4);
        mb.push(make_msg(1, 10)).unwrap();
        mb.push(make_msg(2, 20)).unwrap();
        mb.push(make_msg(3, 30)).unwrap();
        let snapshot = mb.drain();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].behavior_id, 1);
        assert_eq!(snapshot[1].behavior_id, 2);
        assert_eq!(snapshot[2].behavior_id, 3);
        assert_eq!(mb.len(), 3);
        assert_eq!(mb.pop().unwrap().behavior_id, 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 2);
        assert_eq!(mb.pop().unwrap().behavior_id, 3);
    }

    #[test]
    fn test_concurrent_push() {
        use std::thread;

        let mb = Arc::new(Mailbox::new(0));
        let mut handles = Vec::new();
        for t in 0..4 {
            let mb_clone = Arc::clone(&mb);
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    mb_clone
                        .push(make_msg((t * 100 + i) as u16, (t * 100 + i) as u64))
                        .unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(mb.len(), 400);
        let mut mb = Arc::try_unwrap(mb).unwrap_or_else(|_| panic!("Arc still has live clones"));
        let mut count = 0;
        while mb.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, 400);
    }

    #[test]
    fn test_receive_match_preserves_skipped_order() {
        let mut mb = Mailbox::new(4);
        mb.push(make_msg(1, 100)).unwrap();
        mb.push(make_msg(2, 200)).unwrap();
        mb.push(make_msg(3, 300)).unwrap();
        let found = mb.receive_match(&[2]);
        assert_eq!(found, Some((0, Arc::new(vec![Value::int(42)]))));
        assert!(mb.commit_receive_match().is_some());
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.pop().unwrap().behavior_id, 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 3);
        assert!(mb.is_empty());
    }

    #[test]
    fn guard_rejection_then_success_commits_only_successful_candidate() {
        let mut mb = Mailbox::new(0);
        mb.push(make_msg_with(2, 100, 11, MessagePriority::Normal))
            .unwrap();
        mb.push(make_msg_with(2, 200, 22, MessagePriority::Normal))
            .unwrap();

        let first = mb.receive_match(&[2]).unwrap();
        assert_eq!(first.1[0], Value::int(11));
        // Simulate pattern/guard rejection: retry without commit.
        let second = mb.receive_match(&[2]).unwrap();
        assert_eq!(second.1[0], Value::int(22));

        let committed = mb.commit_receive_match().unwrap();
        assert_eq!(committed[0], Value::int(22));
        assert_eq!(mb.len(), 1);
        let remaining = mb.pop().unwrap();
        assert_eq!(remaining.sender, 100);
        assert_eq!(remaining.payload[0], Value::int(11));
    }

    #[test]
    fn reset_preserves_rejected_candidates_and_count() {
        let mut mb = Mailbox::new(0);
        mb.push(make_msg_with(2, 100, 11, MessagePriority::Normal))
            .unwrap();
        mb.push(make_msg_with(2, 200, 22, MessagePriority::Normal))
            .unwrap();

        assert_eq!(mb.receive_match(&[2]).unwrap().1[0], Value::int(11));
        assert_eq!(mb.receive_match(&[2]).unwrap().1[0], Value::int(22));
        assert!(mb.receive_match(&[2]).is_none());
        assert_eq!(mb.len(), 2);
        mb.reset_receive_match();
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.receive_match(&[2]).unwrap().1[0], Value::int(11));
    }

    #[test]
    fn local_system_and_normal_candidates_share_commit_lifecycle() {
        let mut mb = Mailbox::new(0);
        mb.push_local(make_msg_with(7, 1, 10, MessagePriority::Normal))
            .unwrap();
        mb.push(make_msg_with(7, 2, 20, MessagePriority::System))
            .unwrap();
        mb.push(make_msg_with(7, 3, 30, MessagePriority::Normal))
            .unwrap();

        // System priority wins, but the message remains counted until commit.
        let system = mb.receive_match(&[7]).unwrap();
        assert_eq!(system.1[0], Value::int(20));
        assert_eq!(mb.len(), 3);
        assert_eq!(mb.commit_receive_match().unwrap()[0], Value::int(20));
        assert_eq!(mb.len(), 2);

        // Local normal was staged before the concurrently queued normal.
        let local = mb.receive_match(&[7]).unwrap();
        assert_eq!(local.1[0], Value::int(10));
        assert_eq!(mb.commit_receive_match().unwrap()[0], Value::int(10));
        assert_eq!(mb.len(), 1);

        let normal = mb.receive_match(&[7]).unwrap();
        assert_eq!(normal.1[0], Value::int(30));
        assert_eq!(mb.commit_receive_match().unwrap()[0], Value::int(30));
        assert!(mb.is_empty());
    }
}
