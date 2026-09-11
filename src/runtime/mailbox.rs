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
    /// Non-matching normal messages staged during selective receive.
    /// The boolean marks candidates already tried by the current receive.
    skip_buffer: VecDeque<(Message, bool)>,
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
            skip_buffer: VecDeque::new(),
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
        let result = self
            .system_queue
            .pop()
            .or_else(|| self.local_queue.pop_front())
            .or_else(|| self.skip_buffer.pop_front().map(|(m, _)| m))
            .or_else(|| self.normal_queue.pop());
        if result.is_some() {
            self.release_slot();
        }
        result
    }

    /// Selective receive: scan for the first message whose behavior id
    /// appears in `behavior_ids`.
    ///
    /// This intentionally preserves the existing selective-receive lifecycle:
    /// local/system candidates are consumed when returned, while normal
    /// candidates staged in `skip_buffer` are removed by
    /// `commit_receive_match`. Transactional guard handling is a separate
    /// runtime/ORCA concern and is not changed here.
    pub fn receive_match(&mut self, behavior_ids: &[u16]) -> Option<(usize, Arc<Vec<Value>>)> {
        // 1. Scan scheduler-local messages first.
        for i in 0..self.local_queue.len() {
            let bid = self.local_queue[i].behavior_id;
            if let Some(pos) = behavior_ids.iter().position(|&id| id == bid) {
                let msg = self.local_queue.remove(i).expect("mailbox index was valid");
                self.release_slot();
                return Some((pos, msg.payload));
            }
        }

        // Stage unmatched local traffic. System messages retain priority;
        // normal/bulk messages enter the selective-receive skip buffer.
        while let Some(msg) = self.local_queue.pop_front() {
            if msg.priority == MessagePriority::System {
                self.system_queue.push(msg);
            } else {
                self.skip_buffer.push_back((msg, false));
            }
        }

        // 2. Scan system queue.
        if let Some(result) = Self::scan_queue(&self.system_queue, behavior_ids) {
            self.release_slot();
            return Some(result);
        }

        // 3. Try already staged normal messages.
        for i in 0..self.skip_buffer.len() {
            let (tried, bid) = (self.skip_buffer[i].1, self.skip_buffer[i].0.behavior_id);
            if !tried {
                if let Some(pos) = behavior_ids.iter().position(|&id| id == bid) {
                    self.skip_buffer[i].1 = true;
                    return Some((pos, Arc::clone(&self.skip_buffer[i].0.payload)));
                }
            }
        }

        // 4. Drain newly arrived normal messages into the skip buffer.
        while let Some(msg) = self.normal_queue.pop() {
            self.skip_buffer.push_back((msg, false));
        }
        for i in 0..self.skip_buffer.len() {
            let (tried, bid) = (self.skip_buffer[i].1, self.skip_buffer[i].0.behavior_id);
            if !tried {
                if let Some(pos) = behavior_ids.iter().position(|&id| id == bid) {
                    self.skip_buffer[i].1 = true;
                    return Some((pos, Arc::clone(&self.skip_buffer[i].0.payload)));
                }
            }
        }
        None
    }

    /// Drain and scan a concurrent queue for a matching message.
    fn scan_queue(
        queue: &SegQueue<Message>,
        behavior_ids: &[u16],
    ) -> Option<(usize, Arc<Vec<Value>>)> {
        let mut drained: Vec<Message> = Vec::new();
        while let Some(msg) = queue.pop() {
            drained.push(msg);
        }
        let mut found = None;
        let mut requeue: Vec<Message> = Vec::with_capacity(drained.len());
        for msg in drained {
            if found.is_none() {
                if let Some(pos) = behavior_ids.iter().position(|&id| id == msg.behavior_id) {
                    found = Some((pos, msg.payload));
                    continue;
                }
            }
            requeue.push(msg);
        }
        for msg in requeue {
            queue.push(msg);
        }
        found
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
        let mut snapshot = Vec::with_capacity(self.len());
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
        while let Some((msg, _)) = self.skip_buffer.pop_front() {
            self.normal_queue.push(msg);
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Commit the first candidate tried by the existing selective-receive
    /// protocol. The candidate remains counted until it is physically removed.
    pub fn commit_receive_match(&mut self) {
        if let Some(idx) = self.skip_buffer.iter().position(|(_, tried)| *tried) {
            self.skip_buffer.remove(idx);
            self.release_slot();
        }
        for (_, tried) in self.skip_buffer.iter_mut() {
            *tried = false;
        }
    }

    pub fn reset_receive_match(&mut self) {
        for (_, tried) in self.skip_buffer.iter_mut() {
            *tried = false;
        }
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
}
