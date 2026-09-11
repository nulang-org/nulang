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
enum ReservedMatch {
    System(usize),
    Normal(usize),
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
    /// System-priority messages staged during selective receive. They remain
    /// logically queued until an explicit `ReceiveCommit`.
    system_skip_buffer: VecDeque<(Message, bool)>,
    /// Normal/bulk messages staged during selective receive. The boolean marks
    /// candidates already rejected by the current receive expression.
    skip_buffer: VecDeque<(Message, bool)>,
    /// Candidate returned by the most recent `receive_match`. Calling
    /// `receive_match` again before commit means this candidate failed its
    /// pattern/guard; it remains queued with its tried bit set.
    reserved_match: Option<ReservedMatch>,
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
            reserved_match: None,
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
        self.reserved_match = None;
        let result = self
            .system_skip_buffer
            .pop_front()
            .map(|(m, _)| m)
            .or_else(|| self.system_queue.pop())
            .or_else(|| self.local_queue.pop_front())
            .or_else(|| self.skip_buffer.pop_front().map(|(m, _)| m))
            .or_else(|| self.normal_queue.pop());
        if result.is_some() {
            self.release_slot();
        }
        self.clear_receive_attempts();
        result
    }

    /// Selective receive: reserve the first matching message without
    /// consuming it.
    ///
    /// Calling this method again before `commit_receive_match` means the
    /// previously reserved candidate failed its pattern/guard. That candidate
    /// remains queued with its `tried` bit set so the scan advances to the next
    /// eligible message. Ownership is transferred only at commit time.
    pub fn receive_match(&mut self, behavior_ids: &[u16]) -> Option<(usize, Arc<Vec<Value>>)> {
        // A retry is proof that the prior reservation was rejected. Keep its
        // tried bit set, but no candidate is currently eligible for commit.
        self.reserved_match = None;

        // Preserve system priority while making every source transactional.
        // Moving between physical queues does not change queued_count because
        // no logical message has been consumed.
        while let Some(msg) = self.system_queue.pop() {
            self.system_skip_buffer.push_back((msg, false));
        }
        while let Some(msg) = self.local_queue.pop_front() {
            if msg.priority == MessagePriority::System {
                self.system_skip_buffer.push_back((msg, false));
            } else {
                self.skip_buffer.push_back((msg, false));
            }
        }

        if let Some((arm_idx, candidate_idx, payload)) =
            Self::reserve_in_buffer(&mut self.system_skip_buffer, behavior_ids)
        {
            self.reserved_match = Some(ReservedMatch::System(candidate_idx));
            return Some((arm_idx, payload));
        }

        while let Some(msg) = self.normal_queue.pop() {
            self.skip_buffer.push_back((msg, false));
        }
        if let Some((arm_idx, candidate_idx, payload)) =
            Self::reserve_in_buffer(&mut self.skip_buffer, behavior_ids)
        {
            self.reserved_match = Some(ReservedMatch::Normal(candidate_idx));
            return Some((arm_idx, payload));
        }

        None
    }

    fn reserve_in_buffer(
        buffer: &mut VecDeque<(Message, bool)>,
        behavior_ids: &[u16],
    ) -> Option<(usize, usize, Arc<Vec<Value>>)> {
        for idx in 0..buffer.len() {
            let (tried, behavior_id) = (buffer[idx].1, buffer[idx].0.behavior_id);
            if tried {
                continue;
            }
            if let Some(arm_idx) = behavior_ids.iter().position(|&id| id == behavior_id) {
                buffer[idx].1 = true;
                return Some((arm_idx, idx, Arc::clone(&buffer[idx].0.payload)));
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
        self.reserved_match = None;
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

    /// Return staged selective-receive messages to their concurrent queues.
    /// Called at actor turn boundaries so the next turn starts with clean
    /// reservation state.
    pub fn flush_skip_buffer(&mut self) {
        self.reserved_match = None;
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

    /// Commit exactly the candidate reserved by the last successful scan.
    /// Returns its payload so the runtime can establish receiver-side ORCA
    /// ownership exactly once, after pattern and guard evaluation succeed.
    pub fn commit_receive_match(&mut self) -> Option<Arc<Vec<Value>>> {
        let reserved = self.reserved_match.take()?;
        let removed = match reserved {
            ReservedMatch::System(idx) => self.system_skip_buffer.remove(idx),
            ReservedMatch::Normal(idx) => self.skip_buffer.remove(idx),
        };
        let Some((msg, _)) = removed else {
            self.clear_receive_attempts();
            return None;
        };
        let payload = Arc::clone(&msg.payload);
        self.release_slot();
        self.clear_receive_attempts();
        Some(payload)
    }

    fn clear_receive_attempts(&mut self) {
        for (_, tried) in self.system_skip_buffer.iter_mut() {
            *tried = false;
        }
        for (_, tried) in self.skip_buffer.iter_mut() {
            *tried = false;
        }
    }

    pub fn reset_receive_match(&mut self) {
        self.reserved_match = None;
        self.clear_receive_attempts();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(behavior_id: u16, sender: u64) -> Message {
        Message {
            behavior_id,
            payload: Arc::new(vec![Value::int(42)]),
            sender,
            priority: MessagePriority::Normal,
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
        mb.commit_receive_match();
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.pop().unwrap().behavior_id, 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 3);
        assert!(mb.is_empty());
    }

    #[test]
    fn rejected_candidate_remains_when_later_candidate_commits() {
        let mut mb = Mailbox::new(8);
        mb.push(make_msg(1, 10)).unwrap();
        mb.push(make_msg(2, 20)).unwrap();
        mb.push(make_msg(3, 30)).unwrap();

        // First candidate is returned but its guard is assumed to fail.
        assert_eq!(mb.receive_match(&[1, 2]).map(|(arm, _)| arm), Some(0));
        assert_eq!(mb.len(), 3);

        // Retrying reserves the next candidate; commit consumes only it.
        assert_eq!(mb.receive_match(&[1, 2]).map(|(arm, _)| arm), Some(1));
        assert!(mb.commit_receive_match().is_some());
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.pop().unwrap().behavior_id, 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 3);
    }

    #[test]
    fn reset_reexposes_reserved_candidate_without_consuming_it() {
        let mut mb = Mailbox::new(4);
        mb.push(make_msg(7, 1)).unwrap();
        assert_eq!(mb.receive_match(&[7]).map(|(arm, _)| arm), Some(0));
        assert_eq!(mb.len(), 1);
        mb.reset_receive_match();
        assert_eq!(mb.receive_match(&[7]).map(|(arm, _)| arm), Some(0));
        assert_eq!(mb.len(), 1);
    }

    #[test]
    fn system_candidate_is_reserved_until_commit() {
        let mut mb = Mailbox::new(1);
        let mut msg = make_msg(9, 1);
        msg.priority = MessagePriority::System;
        mb.push(msg).unwrap();

        assert_eq!(mb.receive_match(&[9]).map(|(arm, _)| arm), Some(0));
        assert_eq!(mb.len(), 1);
        assert!(mb.commit_receive_match().is_some());
        assert_eq!(mb.len(), 0);
        assert!(mb.pop().is_none());
    }
}
