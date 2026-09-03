//! MPSC mailbox with priority bands and optional capacity limit.
//!
//! Two priority bands (`System` and `Normal`/`Bulk`) ensure that supervisor
//! exit signals and monitor DOWN messages are never delayed behind a queue
//! of regular application messages.  When a capacity limit is configured,
//! `System` messages always bypass the limit — preserving BEAM/OTP
//! reliability guarantees — while `Normal` and `Bulk` messages are
//! rejected with backpressure when the mailbox is full.
//!
//! Uses `crossbeam::queue::SegQueue` (lock-free, unbounded segments) for
//! each band.  Memory is reclaimed via crossbeam's epoch-based garbage
//! collection.

use crate::vm::Value;
use crossbeam::queue::SegQueue;
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Message sent between actors.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub behavior_id: u16,
    /// Payload values, shared via `Arc` to avoid cloning on every
    /// `receive_match` scan. The VM never mutates incoming payloads,
    /// so `Arc` is safe.
    pub payload: Arc<Vec<Value>>,
    pub sender: u64, // Actor ID of sender
    pub priority: MessagePriority,
    /// W3C traceparent for distributed tracing. When set, the receiver's
    /// scheduler creates a child span linked to the sender's trace so
    /// causal chains span actor and node boundaries.
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePriority {
    System = 0, // Urgent (failure signals, monitoring)
    Normal = 1, // Regular messages
    Bulk = 2,   // Bulk/non-urgent
}

/// What happens to a `Normal`/`Bulk` message when the mailbox is at
/// capacity. `System` messages always bypass the limit under every policy,
/// preserving BEAM/OTP reliability guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MailboxOverflowPolicy {
    /// Reject the incoming message: return `Err(msg)` and count it in
    /// `rejected()`. The sender can decide what to do (the runtime's
    /// fallback is to drop it, as before).
    #[default]
    Reject,
    /// Drop the oldest normal message already queued and accept the new
    /// one. The dropped message is counted in `dropped_oldest()`.
    DropOldest,
}

/// MPSC mailbox with priority bands and optional capacity.
///
/// Two `SegQueue` instances provide priority ordering without starving
/// normal messages: every `pop` / `receive_match` drains the system band
/// completely before touching the normal band.
///
/// When `capacity > 0`, `push` rejects `Normal` and `Bulk` messages once
/// the total message count reaches the limit.  `System` messages always
/// succeed, preserving BEAM/OTP reliability guarantees.
///
/// All methods that access the skip-buffer take `&mut self` because they
/// run exclusively on the single scheduler thread — no `RefCell` needed.
///
/// Padded to a cache line so the two `SegQueue`s (pushed from arbitrary
/// sender threads, popped on the scheduler thread) don't share a line with
/// the owning `Actor`'s other fields.
#[repr(align(64))]
pub struct Mailbox {
    system_queue: SegQueue<Message>,
    normal_queue: SegQueue<Message>,
    /// Same-thread local queue: messages pushed from the scheduler thread
    /// itself (same-shard sends, exit notifications, DLQ) bypass SegQueue
    /// atomics. Network-thread pushes still go through normal_queue.
    ///
    /// SAFETY: Only accessed from the scheduler thread — `push_local` (write)
    /// and all read methods (`is_empty`, `len`, `pop`, `receive_match`,
    /// `drain`) run on the single scheduler thread. Network-thread `push`
    /// never touches this field.
    local_queue: UnsafeCell<VecDeque<Message>>,
    capacity: usize,
    overflow_policy: MailboxOverflowPolicy,
    /// Messages rejected at capacity under `Reject` (across both push paths).
    rejected_count: AtomicU64,
    /// Normal messages dropped to make room under `DropOldest`.
    dropped_oldest_count: AtomicU64,
    /// Skip-buffer for non-matching normal messages drained during selective
    /// receive (`receive_match`). Messages stay here in FIFO order until a
    /// later `receive_match` finds a match. System messages are NOT placed
    /// here — they are scanned directly from `system_queue`.
    skip_buffer: VecDeque<(Message, bool)>,
}

// SAFETY: `Mailbox` is `Sync` because mutable fields (`local_queue`,
// `skip_buffer`) are accessed exclusively from the scheduler thread
// (all `&mut self` methods run within `step_actor`/`ReceiveMatch`/etc
// on the single scheduler thread).  The `SegQueue` fields are `Sync`
// (lock-free concurrent queues) and may be safely pushed from network
// threads via `&self` methods.  `local_queue` is wrapped in `UnsafeCell`
// so that `&self` read methods (`is_empty`, `len`) can inspect it without
// a mutable borrow; `UnsafeCell<T>: Sync` for `Send` T (and `VecDeque<Message>: Send`).
unsafe impl Sync for Mailbox {}

impl Mailbox {
    // --- internal unsafe accessors ---
    // SAFETY: these are always called from the scheduler thread, and
    // `&mut self` callers prove exclusive access. `&self` callers
    // (is_empty, len) only read, which is safe because no concurrent
    // mutation occurs.
    fn local_queue_ref(&self) -> &VecDeque<Message> {
        unsafe { &*self.local_queue.get() }
    }
    fn local_queue_mut(&mut self) -> &mut VecDeque<Message> {
        unsafe { &mut *self.local_queue.get() }
    }

    /// Create a new mailbox.
    ///
    /// `capacity`: maximum total messages allowed.  `0` = unbounded
    /// (BEAM/OTP semantics).  `System` messages always bypass the limit.
    pub fn new(capacity: usize) -> Self {
        Mailbox::with_policy(capacity, MailboxOverflowPolicy::Reject)
    }

    /// Create a mailbox with a capacity and an overflow policy.
    ///
    /// `capacity`: maximum total messages allowed.  `0` = unbounded
    /// (BEAM/OTP semantics).  `System` messages always bypass the limit.
    pub fn with_policy(capacity: usize, policy: MailboxOverflowPolicy) -> Self {
        Mailbox {
            system_queue: SegQueue::new(),
            normal_queue: SegQueue::new(),
            local_queue: UnsafeCell::new(VecDeque::new()),
            capacity,
            overflow_policy: policy,
            rejected_count: AtomicU64::new(0),
            dropped_oldest_count: AtomicU64::new(0),
            skip_buffer: VecDeque::new(),
        }
    }

    /// Reconfigure the capacity and overflow policy. Scheduler-thread only
    /// (call before the mailbox is shared with senders).
    pub fn set_bounds(&mut self, capacity: usize, policy: MailboxOverflowPolicy) {
        self.capacity = capacity;
        self.overflow_policy = policy;
    }

    /// Configured overflow policy.
    pub fn overflow_policy(&self) -> MailboxOverflowPolicy {
        self.overflow_policy
    }

    /// Number of messages rejected at capacity under the `Reject` policy.
    pub fn rejected(&self) -> u64 {
        self.rejected_count.load(Ordering::Relaxed)
    }

    /// Number of normal messages dropped to make room under `DropOldest`.
    pub fn dropped_oldest(&self) -> u64 {
        self.dropped_oldest_count.load(Ordering::Relaxed)
    }

    /// Push a message into the mailbox.
    ///
    /// `System` messages always succeed.  `Normal` and `Bulk` messages are
    /// rejected with `Err(msg)` when the mailbox is at capacity (a
    /// non-zero `capacity` was configured and both queues together hold
    /// that many messages).
    pub fn push(&self, msg: Message) -> Result<(), Message> {
        if msg.priority == MessagePriority::System {
            self.system_queue.push(msg);
            return Ok(());
        }
        if self.capacity > 0 && self.len() >= self.capacity {
            match self.overflow_policy {
                MailboxOverflowPolicy::Reject => {
                    self.rejected_count.fetch_add(1, Ordering::Relaxed);
                    return Err(msg);
                }
                MailboxOverflowPolicy::DropOldest => {
                    // Cross-thread path: only the normal queue is touchable
                    // from `&self`. Drop its oldest message; if it is empty
                    // (all queued normal messages live in the scheduler-thread
                    // local queue / skip buffer), fall back to rejecting.
                    if self.normal_queue.pop().is_none() {
                        self.rejected_count.fetch_add(1, Ordering::Relaxed);
                        return Err(msg);
                    }
                    self.dropped_oldest_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.normal_queue.push(msg);
        Ok(())
    }

    /// Push a message from the same thread (scheduler).
    ///
    /// Messages pushed via `push_local` bypass the lock-free `SegQueue`
    /// atomics and land directly in a thread-local `VecDeque`, drained
    /// before the concurrent queues on every `pop` / `receive_match`.
    /// This is the hot path for same-shard actor-to-actor messaging.
    ///
    /// `System` messages always succeed.  `Normal` and `Bulk` messages
    /// are rejected with `Err(msg)` at capacity (same policy as `push`).
    pub fn push_local(&mut self, msg: Message) -> Result<(), Message> {
        if msg.priority == MessagePriority::System {
            self.local_queue_mut().push_back(msg);
            return Ok(());
        }
        if self.capacity > 0 && self.len() >= self.capacity {
            match self.overflow_policy {
                MailboxOverflowPolicy::Reject => {
                    self.rejected_count.fetch_add(1, Ordering::Relaxed);
                    return Err(msg);
                }
                MailboxOverflowPolicy::DropOldest => {
                    // Scheduler thread: drop the oldest NORMAL message.
                    // System messages are never dropped. The local queue can
                    // hold System messages (push_local bypasses the capacity
                    // check for System), so exhaustively scan the FRONT of
                    // the queue (rotating System entries to the back) until
                    // the first Normal/Bulk is found — no System is evicted.
                    // The skip buffer and cross-thread normal queue hold only
                    // Normal/Bulk messages, so their fronts are safe.
                    let dropped = self
                        .local_queue_mut()
                        .iter()
                        .position(|m| m.priority != MessagePriority::System)
                        .map(|idx| self.local_queue_mut().remove(idx).unwrap())
                        .or_else(|| self.skip_buffer.pop_front().map(|(m, _)| m))
                        .or_else(|| self.normal_queue.pop());
                    if dropped.is_none() {
                        self.rejected_count.fetch_add(1, Ordering::Relaxed);
                        return Err(msg);
                    }
                    self.dropped_oldest_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.local_queue_mut().push_back(msg);
        Ok(())
    }

    /// Pop the highest-priority message.
    ///
    /// Checks the system queue first (priority), then the same-thread
    /// local queue, then the skip-buffer (non-matching normal messages
    /// staged during a prior `receive_match`), then the normal queue.
    pub fn pop(&mut self) -> Option<Message> {
        self.system_queue
            .pop()
            .or_else(|| self.local_queue_mut().pop_front())
            .or_else(|| self.skip_buffer.pop_front().map(|(m, _)| m))
            .or_else(|| self.normal_queue.pop())
    }

    /// Selective receive: scan for the first message whose behavior id
    /// appears in `behavior_ids`.
    ///
    /// Scan order: same-thread local queue, system queue (network-thread,
    /// rare), skip-buffer (staged normal), then normal queue. Non-matching
    /// local-queue messages are moved into the skip-buffer (system messages
    /// keep priority by routing to system_queue).
    pub fn receive_match(&mut self, behavior_ids: &[u16]) -> Option<(usize, Arc<Vec<Value>>)> {
        // 1. Scan same-thread local queue (fast, fresh).
        for i in 0..self.local_queue_ref().len() {
            let bid = self.local_queue_ref()[i].behavior_id;
            if let Some(pos) = behavior_ids.iter().position(|&id| id == bid) {
                let msg = self.local_queue_mut().remove(i).unwrap();
                return Some((pos, msg.payload));
            }
        }
        // No match in local queue: drain it — system messages go to
        // system_queue (priority preserved), normal messages go to
        // skip_buffer (scanned in subsequent receive_match calls).
        while let Some(msg) = self.local_queue_mut().pop_front() {
            if msg.priority == MessagePriority::System {
                self.system_queue.push(msg);
            } else {
                self.skip_buffer.push_back((msg, false));
            }
        }
        // 2. Scan system queue (small, rare — drain-scan-requeue is fine).
        if let Some(result) = Self::scan_queue(&self.system_queue, behavior_ids) {
            return Some(result);
        }
        // 3. Try the skip-buffer (includes ex-local-queue messages).
        for i in 0..self.skip_buffer.len() {
            let (tried, bid) = (self.skip_buffer[i].1, self.skip_buffer[i].0.behavior_id);
            if !tried {
                if let Some(pos) = behavior_ids.iter().position(|&id| id == bid) {
                    self.skip_buffer[i].1 = true; // mark tried
                    return Some((pos, Arc::clone(&self.skip_buffer[i].0.payload)));
                }
            }
        }
        // 4. Drain the normal queue into the buffer, then scan again.
        while let Some(msg) = self.normal_queue.pop() {
            self.skip_buffer.push_back((msg, false));
        }
        for i in 0..self.skip_buffer.len() {
            let (tried, bid) = (self.skip_buffer[i].1, self.skip_buffer[i].0.behavior_id);
            if !tried {
                if let Some(pos) = behavior_ids.iter().position(|&id| id == bid) {
                    self.skip_buffer[i].1 = true; // mark tried
                    return Some((pos, Arc::clone(&self.skip_buffer[i].0.payload)));
                }
            }
        }
        None
    }

    /// Drain and scan a single queue for a matching message.  Used for the
    /// system queue only (small, rare); the normal queue uses the skip-buffer.
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

    /// Total message count across all queues (approximate — concurrent
    /// queue lengths are snapshots).  Includes the same-thread local queue.
    pub fn len(&self) -> usize {
        self.system_queue.len()
            + self.local_queue_ref().len()
            + self.skip_buffer.len()
            + self.normal_queue.len()
    }

    /// True when all queues and the skip-buffer are empty.
    pub fn is_empty(&self) -> bool {
        self.system_queue.is_empty()
            && self.local_queue_ref().is_empty()
            && self.skip_buffer.is_empty()
            && self.normal_queue.is_empty()
    }

    /// Drain all queues (in priority/FIFO order) into a cloned snapshot,
    /// then restore all messages.
    pub fn drain(&mut self) -> Vec<Message> {
        let mut snapshot = Vec::with_capacity(self.len());
        // Drain system first.
        while let Some(msg) = self.system_queue.pop() {
            snapshot.push(msg);
        }
        // Same-thread local queue.
        while let Some(msg) = self.local_queue_mut().pop_front() {
            snapshot.push(msg);
        }
        // Then skip-buffer (normal messages staged during selective receive).
        while let Some((msg, _)) = self.skip_buffer.pop_front() {
            snapshot.push(msg);
        }
        // Then normal queue.
        while let Some(msg) = self.normal_queue.pop() {
            snapshot.push(msg);
        }
        // Restore: system → system_queue, normal → local_queue.
        for msg in &snapshot {
            if msg.priority == MessagePriority::System {
                self.system_queue.push(msg.clone());
            } else {
                self.local_queue_mut().push_back(msg.clone());
            }
        }
        snapshot
    }

    /// Return all skip-buffer messages to `normal_queue`, then clear the buffer.
    pub fn flush_skip_buffer(&mut self) {
        while let Some((msg, _)) = self.skip_buffer.pop_front() {
            self.normal_queue.push(msg);
        }
    }

    /// Return the configured capacity (0 = unbounded).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Commit a selective receive: remove the first "tried" message from
    /// the skip-buffer and clear remaining "tried" flags. Called after a
    /// pattern+guard check succeeds.
    pub fn commit_receive_match(&mut self) {
        // Remove the first tried entry.
        if let Some(idx) = self.skip_buffer.iter().position(|(_, tried)| *tried) {
            self.skip_buffer.remove(idx);
        }
        // Clear remaining tried flags.
        for (_, tried) in self.skip_buffer.iter_mut() {
            *tried = false;
        }
    }

    /// Reset "tried" flags in the skip-buffer. Called when
    /// `receive_match` returns `None`, preparing the buffer for the next
    /// receive expression.
    pub fn reset_receive_match(&mut self) {
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

    /// Helper to build a test message with minimal boilerplate.
    fn make_msg(behavior_id: u16, sender: u64) -> Message {
        Message {
            behavior_id,
            payload: Arc::new(vec![Value::int(42)]),
            sender,
            priority: MessagePriority::Normal,
            trace_id: None,
        }
    }

    /// Helper to build a System-priority test message.
    fn make_system_msg(behavior_id: u16, sender: u64) -> Message {
        Message {
            behavior_id,
            payload: Arc::new(vec![Value::int(0)]),
            sender,
            priority: MessagePriority::System,
            trace_id: None,
        }
    }

    // Overflow policy: Reject (default).

    #[test]
    fn test_reject_policy_returns_err_and_counts() {
        let mb = Mailbox::with_policy(2, MailboxOverflowPolicy::Reject);
        assert_eq!(mb.overflow_policy(), MailboxOverflowPolicy::Reject);
        assert!(mb.push(make_msg(1, 1)).is_ok());
        assert!(mb.push(make_msg(2, 2)).is_ok());
        let over = mb.push(make_msg(3, 3));
        assert!(over.is_err(), "third normal message must be rejected at capacity");
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.rejected(), 1);
        // System messages always bypass the limit.
        assert!(mb.push(make_system_msg(9, 9)).is_ok());
        assert_eq!(mb.len(), 3);
    }

    #[test]
    fn test_reject_policy_push_local_counts() {
        let mut mb = Mailbox::with_policy(1, MailboxOverflowPolicy::Reject);
        assert!(mb.push_local(make_msg(1, 1)).is_ok());
        assert!(mb.push_local(make_msg(2, 2)).is_err());
        assert_eq!(mb.len(), 1);
        assert_eq!(mb.rejected(), 1);
        assert!(mb.push_local(make_system_msg(9, 9)).is_ok());
        assert_eq!(mb.len(), 2);
    }

    // Overflow policy: DropOldest.

    #[test]
    fn test_drop_oldest_drops_oldest_local_and_accepts() {
        let mut mb = Mailbox::with_policy(2, MailboxOverflowPolicy::DropOldest);
        assert!(mb.push_local(make_msg(1, 1)).is_ok());
        assert!(mb.push_local(make_msg(2, 2)).is_ok());
        // Third message at capacity: oldest (behavior 1) is dropped.
        assert!(mb.push_local(make_msg(3, 3)).is_ok());
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.dropped_oldest(), 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 2, "oldest survives");
        assert_eq!(mb.pop().unwrap().behavior_id, 3, "newest accepted");
        assert!(mb.is_empty());
    }

    #[test]
    fn test_drop_oldest_never_drops_system() {
        // Capacity counts ALL queues (local + skip-buffer + cross-thread
        // normal). System messages bypass the capacity check entirely, so
        // push System first along with one Normal (len() == 2 == capacity).
        // A third push (Normal) must evict the FIRST Normal, never the
        // System — the eviction scan skips System messages.
        let mut mb = Mailbox::with_policy(2, MailboxOverflowPolicy::DropOldest);
        assert!(mb.push_local(make_system_msg(9, 9)).is_ok());
        assert!(mb.push_local(make_msg(1, 1)).is_ok());
        assert!(mb.push_local(make_msg(2, 2)).is_ok());
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.dropped_oldest(), 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 9, "system pops first");
        assert_eq!(mb.pop().unwrap().behavior_id, 2, "newest normal survives");
    }

    #[test]
    fn test_drop_oldest_cross_thread_falls_back_to_reject() {
        // &self push (network thread) can only evict the normal queue. When
        // all queued messages live in the scheduler-thread local queue, it
        // falls back to rejecting the incoming message.
        let mut mb = Mailbox::with_policy(1, MailboxOverflowPolicy::DropOldest);
        assert!(mb.push_local(make_msg(1, 1)).is_ok());
        assert!(mb.push(make_msg(2, 2)).is_err(), "cannot evict local-only queue from &self");
        assert_eq!(mb.rejected(), 1);
        assert_eq!(mb.len(), 1);
    }

    #[test]
    fn test_drop_oldest_cross_thread_evicts_normal_queue() {
        let mb = Mailbox::with_policy(1, MailboxOverflowPolicy::DropOldest);
        assert!(mb.push(make_msg(1, 1)).is_ok());
        assert!(mb.push(make_msg(2, 2)).is_ok(), "oldest normal evicted, new accepted");
        assert_eq!(mb.len(), 1);
        assert_eq!(mb.dropped_oldest(), 1);
    }

    #[test]
    fn test_set_bounds_reconfigures() {
        let mut mb = Mailbox::with_policy(5, MailboxOverflowPolicy::Reject);
        assert_eq!(mb.capacity(), 5);
        mb.set_bounds(1, MailboxOverflowPolicy::DropOldest);
        assert_eq!(mb.capacity(), 1);
        assert_eq!(mb.overflow_policy(), MailboxOverflowPolicy::DropOldest);
        assert!(mb.push(make_msg(1, 1)).is_ok());
        assert!(mb.push(make_msg(2, 2)).is_ok(), "DropOldest now applies");
        assert_eq!(mb.len(), 1);
        assert_eq!(mb.dropped_oldest(), 1);
    }

    // Test 1: Basic push/pop round-trip.
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

    // Test 2: Unbounded — push never fails, even with many messages.
    #[test]
    fn test_unbounded_never_fails() {
        let mut mb = Mailbox::new(0); // 0 = unbounded

        for i in 0..10000 {
            let result = mb.push(make_msg(i as u16, i as u64));
            assert!(
                result.is_ok(),
                "push {} should never fail on unbounded queue",
                i
            );
        }
        assert_eq!(mb.len(), 10000);

        // Pop all messages
        for i in 0..10000 {
            let msg = mb.pop().expect(&format!("pop {} should succeed", i));
            assert_eq!(msg.behavior_id, i as u16);
        }
        assert!(mb.is_empty());
    }

    #[test]
    fn test_supervisor_signals_never_dropped() {
        let mut mb = Mailbox::new(4);

        // Flood with system-priority exit signals
        for i in 0..1000 {
            let signal = Message {
                behavior_id: 0, // System message
                payload: Arc::new(vec![Value::int(i)]),
                sender: i as u64,
                priority: MessagePriority::System,
                trace_id: None,
            };
            mb.push(signal).unwrap();
        }

        // All 1000 signals must be present
        assert_eq!(mb.len(), 1000);

        // Verify every signal is recoverable
        let mut count = 0;
        while mb.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, 1000, "no supervisor signals should be lost");
    }

    // Test 4: len and is_empty track correctly across operations.
    #[test]
    fn test_len_and_is_empty() {
        let mut mb = Mailbox::new(4);
        assert!(mb.is_empty());
        assert_eq!(mb.len(), 0);

        mb.push(make_msg(10, 1)).unwrap();
        assert!(!mb.is_empty());
        assert_eq!(mb.len(), 1);

        mb.push(make_msg(20, 2)).unwrap();
        mb.push(make_msg(30, 3)).unwrap();
        assert_eq!(mb.len(), 3);

        mb.pop().unwrap();
        assert_eq!(mb.len(), 2);

        mb.pop().unwrap();
        mb.pop().unwrap();
        assert!(mb.is_empty());
        assert_eq!(mb.len(), 0);
    }
    // Test 5: drain returns a cloned snapshot without removing messages.
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

        // Mailbox should still contain all messages after drain.
        assert_eq!(mb.len(), 3);
        assert_eq!(mb.pop().unwrap().behavior_id, 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 2);
        assert_eq!(mb.pop().unwrap().behavior_id, 3);
    }
    #[test]
    fn test_concurrent_push() {
        use std::sync::Arc;
        use std::thread;

        let mb = Arc::new(Mailbox::new(0)); // 0 = unbounded for concurrent test
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

        // All 400 messages should be present
        assert_eq!(mb.len(), 400);

        // Recover the owned Mailbox so we can call &mut self methods.
        let mut mb = Arc::try_unwrap(mb).unwrap_or_else(|_| panic!("Arc still has live clones"));
        let mut count = 0;
        while mb.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, 400);
    }
    // Test 7: receive_match preserves the relative FIFO order of ALL
    // non-matched messages, including those queued behind the match.
    #[test]
    fn test_receive_match_preserves_skipped_order() {
        let mut mb = Mailbox::new(4);
        mb.push(make_msg(1, 100)).unwrap(); // A: skipped (no match)
        mb.push(make_msg(2, 200)).unwrap(); // B: matched
        mb.push(make_msg(3, 300)).unwrap(); // C: queued behind the match

        let found = mb.receive_match(&[2]);
        assert_eq!(found, Some((0, Arc::new(vec![Value::int(42)]))));
        // Commit: remove the matched ("tried") message from the skip-buffer.
        mb.commit_receive_match();

        // The mailbox must still serve A before C: selective receive only
        // removes the matched message, it must not reorder the rest.
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.pop().unwrap().behavior_id, 1);
        assert_eq!(mb.pop().unwrap().behavior_id, 3);
        assert!(mb.is_empty());
    }
}
