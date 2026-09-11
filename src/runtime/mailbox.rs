//! MPSC mailbox with priority bands and optional capacity limit.
//!
//! Two priority bands (`System` and `Normal`/`Bulk`) ensure that supervisor
//! exit signals and monitor DOWN messages are never delayed behind a queue
//! of regular application messages. When a capacity limit is configured,
//! `System` messages always bypass the limit while `Normal` and `Bulk`
//! messages are rejected with backpressure when the mailbox is full.
//!
//! Concurrent producers use lock-free `SegQueue`s through `push(&self)`.
//! Scheduler-local traffic and selective-receive staging use `VecDeque`s
//! behind `&mut self`. Selective receive is transactional: a candidate stays
//! physically queued until `commit_receive_match`, while rejected candidates
//! are merely marked tried for the current receive expression.

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
enum PendingReceive {
    System(usize),
    Local(usize),
    Normal(usize),
}

/// MPSC mailbox with priority bands and optional capacity.
///
/// Concurrent producers may call [`Mailbox::push`] through shared references;
/// all operations touching scheduler-local or selective-receive state require
/// `&mut self`. `queued_count` counts logical messages while they move between
/// queues and selective-receive staging buffers.
///
/// Selective receive uses one commit lifecycle for every source:
///
/// 1. system/local/normal candidates remain queued while patterns/guards run;
/// 2. a returned candidate is marked tried and reserved as `pending_receive`;
/// 3. another scan treats the previous reservation as rejected and skips it;
/// 4. commit removes exactly the current reservation and releases one slot;
/// 5. reset clears attempt state without consuming any message.
#[repr(align(64))]
pub struct Mailbox {
    /// Concurrent system-message ingress.
    system_queue: SegQueue<Message>,
    /// Scheduler-owned system staging. Local system messages land here
    /// directly; concurrent system messages are drained here before scans.
    system_buffer: VecDeque<(Message, bool)>,
    /// Concurrent normal/bulk ingress.
    normal_queue: SegQueue<Message>,
    /// Same-thread normal/bulk queue. The boolean marks candidates already
    /// tried by the current selective-receive expression.
    local_queue: VecDeque<(Message, bool)>,
    capacity: usize,
    queued_count: AtomicUsize,
    /// Concurrent normal messages staged during selective receive.
    /// The boolean marks candidates already tried by the current receive.
    skip_buffer: VecDeque<(Message, bool)>,
    /// Candidate returned by the most recent `receive_match` call. It is not
    /// physically removed until commit.
    pending_receive: Option<PendingReceive>,
}

impl Mailbox {
    /// Create a new mailbox.
    ///
    /// `capacity`: maximum total messages allowed. `0` = unbounded.
    /// `System` messages always bypass the limit.
    pub fn new(capacity: usize) -> Self {
        Mailbox {
            system_queue: SegQueue::new(),
            system_buffer: VecDeque::new(),
            normal_queue: SegQueue::new(),
            local_queue: VecDeque::new(),
            capacity,
            queued_count: AtomicUsize::new(0),
            skip_buffer: VecDeque::new(),
            pending_receive: None,
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
    ///
    /// Local system traffic enters the scheduler-owned system staging buffer
    /// so it retains system priority without paying concurrent queue atomics.
    pub fn push_local(&mut self, msg: Message) -> Result<(), Message> {
        let system = msg.priority == MessagePriority::System;
        if !self.reserve_slot(system) {
            return Err(msg);
        }
        if system {
            self.system_buffer.push_back((msg, false));
        } else {
            self.local_queue.push_back((msg, false));
        }
        Ok(())
    }

    /// Pop the highest-priority message.
    pub fn pop(&mut self) -> Option<Message> {
        // A non-selective receive ends any speculative selective transaction.
        self.reset_receive_match();
        let result = self
            .system_buffer
            .pop_front()
            .map(|(m, _)| m)
            .or_else(|| self.system_queue.pop())
            .or_else(|| self.local_queue.pop_front().map(|(m, _)| m))
            .or_else(|| self.skip_buffer.pop_front().map(|(m, _)| m))
            .or_else(|| self.normal_queue.pop());
        if result.is_some() {
            self.release_slot();
        }
        result
    }

    /// Selective receive: reserve the first untried candidate whose behavior
    /// id appears in `behavior_ids` without consuming it.
    ///
    /// Calling this again before commit means the previous candidate failed a
    /// pattern/guard. Its tried bit remains set, while the new scan searches
    /// the remaining candidates. System messages are considered before
    /// normal/bulk messages; normal local traffic keeps its existing fast-path
    /// preference over already-staged/concurrent normal traffic.
    pub fn receive_match(&mut self, behavior_ids: &[u16]) -> Option<(usize, Arc<Vec<Value>>)> {
        // If the VM calls us again, the previous reservation was rejected by
        // pattern/guard evaluation. It remains queued and tried, but is no
        // longer the candidate that a future commit should consume.
        self.pending_receive = None;

        // Drain concurrent system arrivals into the scheduler-owned staging
        // buffer before scanning. Older staged system messages stay first.
        while let Some(msg) = self.system_queue.pop() {
            self.system_buffer.push_back((msg, false));
        }

        if let Some((idx, arm, payload)) = Self::scan_buffer(&mut self.system_buffer, behavior_ids)
        {
            self.pending_receive = Some(PendingReceive::System(idx));
            return Some((arm, payload));
        }

        if let Some((idx, arm, payload)) = Self::scan_buffer(&mut self.local_queue, behavior_ids) {
            self.pending_receive = Some(PendingReceive::Local(idx));
            return Some((arm, payload));
        }

        if let Some((idx, arm, payload)) = Self::scan_buffer(&mut self.skip_buffer, behavior_ids) {
            self.pending_receive = Some(PendingReceive::Normal(idx));
            return Some((arm, payload));
        }

        // Newly arrived normal messages join the staged FIFO, then get one
        // scan in the same receive attempt.
        while let Some(msg) = self.normal_queue.pop() {
            self.skip_buffer.push_back((msg, false));
        }
        if let Some((idx, arm, payload)) = Self::scan_buffer(&mut self.skip_buffer, behavior_ids) {
            self.pending_receive = Some(PendingReceive::Normal(idx));
            return Some((arm, payload));
        }

        None
    }

    fn scan_buffer(
        buffer: &mut VecDeque<(Message, bool)>,
        behavior_ids: &[u16],
    ) -> Option<(usize, usize, Arc<Vec<Value>>)> {
        for idx in 0..buffer.len() {
            let (msg, tried) = &buffer[idx];
            if *tried {
                continue;
            }
            if let Some(arm) = behavior_ids.iter().position(|&id| id == msg.behavior_id) {
                let payload = Arc::clone(&msg.payload);
                buffer[idx].1 = true;
                return Some((idx, arm, payload));
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
        self.reset_receive_match();
        let mut snapshot = Vec::with_capacity(self.len());
        while let Some((msg, _)) = self.system_buffer.pop_front() {
            snapshot.push(msg);
        }
        while let Some(msg) = self.system_queue.pop() {
            snapshot.push(msg);
        }
        while let Some((msg, _)) = self.local_queue.pop_front() {
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
                self.system_buffer.push_back((msg.clone(), false));
            } else {
                self.local_queue.push_back((msg.clone(), false));
            }
        }
        snapshot
    }

    pub fn flush_skip_buffer(&mut self) {
        self.reset_receive_match();
        while let Some((msg, _)) = self.skip_buffer.pop_front() {
            self.normal_queue.push(msg);
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Commit exactly the candidate returned by the most recent
    /// `receive_match`, removing it from its source buffer and returning the
    /// payload so the runtime can establish receiver-side ownership once.
    pub fn commit_receive_match(&mut self) -> Option<Arc<Vec<Value>>> {
        let pending = self.pending_receive.take()?;
        let removed = match pending {
            PendingReceive::System(idx) => self.system_buffer.remove(idx),
            PendingReceive::Local(idx) => self.local_queue.remove(idx),
            PendingReceive::Normal(idx) => self.skip_buffer.remove(idx),
        };

        let payload = removed.map(|(msg, _)| msg.payload);
        if payload.is_some() {
            self.release_slot();
        }
        self.clear_receive_attempts();
        payload
    }

    /// Clear reservations/attempt markers without consuming any message.
    pub fn reset_receive_match(&mut self) {
        self.pending_receive = None;
        self.clear_receive_attempts();
    }

    fn clear_receive_attempts(&mut self) {
        for (_, tried) in self.system_buffer.iter_mut() {
            *tried = false;
        }
        for (_, tried) in self.local_queue.iter_mut() {
            *tried = false;
        }
        for (_, tried) in self.skip_buffer.iter_mut() {
            *tried = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

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
        assert_eq!(mb.len(), 3, "reservation must not consume the candidate");
        mb.commit_receive_match();
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.pop().unwrap().behavior_id, 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 3);
        assert!(mb.is_empty());
    }

    #[test]
    fn rejected_candidate_stays_queued_and_commit_consumes_current_candidate() {
        let mut mb = Mailbox::new(4);
        mb.push(make_msg_with(2, 100, 10, MessagePriority::Normal))
            .unwrap();
        mb.push(make_msg_with(2, 200, 20, MessagePriority::Normal))
            .unwrap();

        let first = mb.receive_match(&[2]).expect("first candidate");
        assert_eq!(first.1[0].as_int(), Some(10));
        assert_eq!(mb.len(), 2);

        // Calling receive_match again means the first guard rejected it.
        let second = mb.receive_match(&[2]).expect("second candidate");
        assert_eq!(second.1[0].as_int(), Some(20));
        assert_eq!(mb.len(), 2);

        let committed = mb.commit_receive_match().expect("commit second candidate");
        assert_eq!(committed[0].as_int(), Some(20));
        assert_eq!(mb.len(), 1);

        // Commit clears attempt state; the rejected first candidate is still
        // present and visible to the next receive expression.
        let remaining = mb.receive_match(&[2]).expect("rejected candidate retained");
        assert_eq!(remaining.1[0].as_int(), Some(10));
    }

    #[test]
    fn reset_makes_rejected_candidate_visible_without_consuming_it() {
        let mut mb = Mailbox::new(4);
        mb.push(make_msg_with(2, 100, 10, MessagePriority::Normal))
            .unwrap();
        let first = mb.receive_match(&[2]).expect("candidate");
        assert_eq!(first.1[0].as_int(), Some(10));
        assert_eq!(mb.len(), 1);
        mb.reset_receive_match();
        assert_eq!(mb.len(), 1);
        let again = mb.receive_match(&[2]).expect("candidate after reset");
        assert_eq!(again.1[0].as_int(), Some(10));
    }

    #[test]
    fn system_local_and_normal_candidates_share_commit_semantics() {
        let mut mb = Mailbox::new(0);
        mb.push_local(make_msg_with(7, 10, 10, MessagePriority::Normal))
            .unwrap();
        mb.push(make_msg_with(7, 20, 20, MessagePriority::System))
            .unwrap();
        mb.push(make_msg_with(7, 30, 30, MessagePriority::Normal))
            .unwrap();

        // System traffic wins and remains counted until commit.
        let system = mb.receive_match(&[7]).expect("system candidate");
        assert_eq!(system.1[0].as_int(), Some(20));
        assert_eq!(mb.len(), 3);
        mb.commit_receive_match().expect("commit system");
        assert_eq!(mb.len(), 2);

        // Scheduler-local normal traffic keeps the local fast-path preference.
        let local = mb.receive_match(&[7]).expect("local candidate");
        assert_eq!(local.1[0].as_int(), Some(10));
        assert_eq!(mb.len(), 2);
        mb.commit_receive_match().expect("commit local");
        assert_eq!(mb.len(), 1);

        let normal = mb.receive_match(&[7]).expect("normal candidate");
        assert_eq!(normal.1[0].as_int(), Some(30));
        assert_eq!(mb.len(), 1);
        mb.commit_receive_match().expect("commit normal");
        assert!(mb.is_empty());
    }
}
