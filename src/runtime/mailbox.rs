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
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const INLINE_PAYLOAD_VALUES: usize = 4;

/// Actor-message payload optimized for the common small-message case.
///
/// Up to four NaN-boxed Values live directly in the envelope. Larger payloads
/// retain shared Arc<Vec<Value>> storage so cloning a large message stays cheap.
#[derive(Debug, Clone, PartialEq)]
pub enum MessagePayload {
    Inline {
        len: u8,
        values: [Value; INLINE_PAYLOAD_VALUES],
    },
    Shared(Arc<Vec<Value>>),
}

impl MessagePayload {
    #[inline]
    pub fn from_slice(values: &[Value]) -> Self {
        if values.len() <= INLINE_PAYLOAD_VALUES {
            let mut inline = [Value::nil(); INLINE_PAYLOAD_VALUES];
            inline[..values.len()].copy_from_slice(values);
            Self::Inline {
                len: values.len() as u8,
                values: inline,
            }
        } else {
            Self::Shared(Arc::new(values.to_vec()))
        }
    }

    #[inline]
    pub fn from_vec(values: Vec<Value>) -> Self {
        if values.len() <= INLINE_PAYLOAD_VALUES {
            let len = values.len();
            let mut inline = [Value::nil(); INLINE_PAYLOAD_VALUES];
            inline[..len].copy_from_slice(&values);
            Self::Inline {
                len: len as u8,
                values: inline,
            }
        } else {
            Self::Shared(Arc::new(values))
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[Value] {
        match self {
            Self::Inline { len, values } => &values[..*len as usize],
            Self::Shared(values) => values.as_slice(),
        }
    }

    #[inline]
    pub fn to_vec(&self) -> Vec<Value> {
        self.as_slice().to_vec()
    }

    /// Materialize shared ownership only when an API genuinely needs payload
    /// lifetime independent of the message envelope (selective receive).
    #[inline]
    pub fn to_shared(&self) -> Arc<Vec<Value>> {
        match self {
            Self::Inline { .. } => Arc::new(self.as_slice().to_vec()),
            Self::Shared(values) => Arc::clone(values),
        }
    }

    #[inline]
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline { .. })
    }
}

impl Deref for MessagePayload {
    type Target = [Value];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl AsRef<[Value]> for MessagePayload {
    #[inline]
    fn as_ref(&self) -> &[Value] {
        self.as_slice()
    }
}

impl From<Vec<Value>> for MessagePayload {
    fn from(values: Vec<Value>) -> Self {
        Self::from_vec(values)
    }
}

impl From<Arc<Vec<Value>>> for MessagePayload {
    fn from(values: Arc<Vec<Value>>) -> Self {
        if values.len() <= INLINE_PAYLOAD_VALUES {
            Self::from_slice(values.as_slice())
        } else {
            Self::Shared(values)
        }
    }
}

/// Message sent between actors.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub behavior_id: u16,
    /// Payload values: 0–4 inline, larger payloads Arc-backed.
    pub payload: MessagePayload,
    pub sender: u64,
    pub priority: MessagePriority,
    /// W3C traceparent for distributed tracing.
    pub trace_id: Option<String>,
    /// Stable durable-delivery identity when this mailbox entry was already
    /// accepted into the receiver's durable transition log.
    ///
    /// The scheduler uses this marker only to avoid journaling the accepted
    /// command a second time. It is not part of the public wire protocol yet.
    pub durable_id: Option<crate::runtime::persistence::DurableMessageId>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchLane {
    System,
    Local,
    Normal,
}

/// Lazy positional index for one staged selective-receive lane.
///
/// Positions remain stable while a receive transaction only appends arrivals
/// and rejects guards. A successful commit or ordinary pop shifts VecDeque
/// positions and invalidates the index, which is rebuilt lazily.
#[derive(Debug)]
struct ReceiveLaneIndex {
    positions: FxHashMap<u16, Vec<usize>>,
    cursors: FxHashMap<u16, usize>,
    valid: bool,
}

impl ReceiveLaneIndex {
    fn new() -> Self {
        Self {
            positions: FxHashMap::default(),
            cursors: FxHashMap::default(),
            valid: true,
        }
    }

    #[inline]
    fn append(&mut self, behavior_id: u16, index: usize) {
        if self.valid {
            self.positions.entry(behavior_id).or_default().push(index);
        }
    }

    fn invalidate(&mut self) {
        self.positions.clear();
        self.cursors.clear();
        self.valid = false;
    }

    fn reset_cursors(&mut self) {
        self.cursors.clear();
    }

    fn ensure(&mut self, buffer: &VecDeque<(Message, bool)>) {
        if self.valid {
            return;
        }
        self.positions.clear();
        for (idx, (msg, _)) in buffer.iter().enumerate() {
            self.positions.entry(msg.behavior_id).or_default().push(idx);
        }
        self.cursors.clear();
        self.valid = true;
    }

    fn next_candidate(
        &mut self,
        buffer: &VecDeque<(Message, bool)>,
        behavior_ids: &[u16],
    ) -> Option<(usize, usize)> {
        self.ensure(buffer);

        let mut best: Option<(usize, usize)> = None;
        for (arm_pos, &behavior_id) in behavior_ids.iter().enumerate() {
            let Some(positions) = self.positions.get(&behavior_id) else {
                continue;
            };
            let cursor = self.cursors.entry(behavior_id).or_insert(0);
            while *cursor < positions.len() {
                let idx = positions[*cursor];
                match buffer.get(idx) {
                    Some((_, false)) => break,
                    Some((_, true)) | None => *cursor += 1,
                }
            }
            if *cursor >= positions.len() {
                continue;
            }
            let idx = positions[*cursor];
            match best {
                None => best = Some((arm_pos, idx)),
                Some((_, best_idx)) if idx < best_idx => best = Some((arm_pos, idx)),
                _ => {}
            }
        }
        best
    }
}

#[derive(Debug)]
struct ReceiveIndexes {
    system: ReceiveLaneIndex,
    local: ReceiveLaneIndex,
    normal: ReceiveLaneIndex,
}

impl ReceiveIndexes {
    fn new() -> Self {
        Self {
            system: ReceiveLaneIndex::new(),
            local: ReceiveLaneIndex::new(),
            normal: ReceiveLaneIndex::new(),
        }
    }
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
    /// System messages already observed by a selective receive. They remain
    /// logically queued until a successful pattern+guard commits exactly one.
    system_skip_buffer: VecDeque<(Message, bool)>,
    /// Scheduler-local messages staged by selective receive. Keeping this lane
    /// separate preserves their order relative to later local arrivals.
    local_skip_buffer: VecDeque<(Message, bool)>,
    /// Normal messages staged by selective receive.
    skip_buffer: VecDeque<(Message, bool)>,
    /// Selective-receive indexes are allocated lazily so actors that only use
    /// ordinary FIFO receive do not carry three hash maps in every mailbox.
    receive_indexes: Option<Box<ReceiveIndexes>>,
    /// The most recently returned candidate. A second `receive_match` call
    /// means the previous candidate's guard rejected it; only this active
    /// candidate may be consumed by `commit_receive_match`.
    active_match: Option<(MatchLane, usize, Arc<Vec<Value>>)>,
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
            local_skip_buffer: VecDeque::new(),
            skip_buffer: VecDeque::new(),
            receive_indexes: None,
            active_match: None,
        }
    }

    /// Reserve one logical mailbox slot.
    ///
    /// System messages and unbounded mailboxes always reserve successfully.
    /// Bounded normal/bulk traffic uses CAS so concurrent producers cannot all
    /// observe the same free slot and overfill the mailbox.
    fn reserve_slot(&self, system: bool) -> bool {
        // queued_count is capacity/accounting state only; SegQueue owns
        // publication and synchronization for the message itself. Atomic
        // modification order is sufficient to keep bounded producers from
        // over-reserving slots, so acquire/release fences add no ordering
        // guarantee that the mailbox relies on.
        if system || self.capacity == 0 {
            self.queued_count.fetch_add(1, Ordering::Relaxed);
            return true;
        }

        let mut current = self.queued_count.load(Ordering::Relaxed);
        loop {
            if current >= self.capacity {
                return false;
            }
            match self.queued_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn release_slot(&self) {
        let previous = self.queued_count.fetch_sub(1, Ordering::Relaxed);
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

    /// Pop the highest-priority queued message.
    pub fn pop(&mut self) -> Option<Message> {
        // Staged messages predate newly-arrived messages in the same lane, so
        // they must remain ahead of the concurrent queue after a rejected
        // selective receive.
        let result = self
            .system_skip_buffer
            .pop_front()
            .map(|(m, _)| m)
            .or_else(|| self.system_queue.pop())
            .or_else(|| self.local_skip_buffer.pop_front().map(|(m, _)| m))
            .or_else(|| self.local_queue.pop_front())
            .or_else(|| self.skip_buffer.pop_front().map(|(m, _)| m))
            .or_else(|| self.normal_queue.pop());
        if result.is_some() {
            self.active_match = None;
            self.invalidate_receive_indexes();
            self.release_slot();
        }
        result
    }

    fn stage_message(
        buffer: &mut VecDeque<(Message, bool)>,
        index: &mut ReceiveLaneIndex,
        msg: Message,
    ) {
        let position = buffer.len();
        let behavior_id = msg.behavior_id;
        buffer.push_back((msg, false));
        index.append(behavior_id, position);
    }

    fn ensure_receive_indexes(&mut self) {
        if self.receive_indexes.is_none() {
            self.receive_indexes = Some(Box::new(ReceiveIndexes::new()));
        }
    }

    fn stage_arrivals(&mut self) {
        let indexes = self
            .receive_indexes
            .as_mut()
            .expect("selective receive indexes must be initialized");

        // Scheduler-local system messages join the system lane; other local
        // traffic stays in its own lane so its FIFO position is stable.
        while let Some(msg) = self.local_queue.pop_front() {
            if msg.priority == MessagePriority::System {
                Self::stage_message(&mut self.system_skip_buffer, &mut indexes.system, msg);
            } else {
                Self::stage_message(&mut self.local_skip_buffer, &mut indexes.local, msg);
            }
        }
        while let Some(msg) = self.system_queue.pop() {
            Self::stage_message(&mut self.system_skip_buffer, &mut indexes.system, msg);
        }
        while let Some(msg) = self.normal_queue.pop() {
            Self::stage_message(&mut self.skip_buffer, &mut indexes.normal, msg);
        }
    }

    fn scan_indexed(
        buffer: &mut VecDeque<(Message, bool)>,
        index: &mut ReceiveLaneIndex,
        behavior_ids: &[u16],
    ) -> Option<(usize, usize, Arc<Vec<Value>>)> {
        let (arm_pos, message_idx) = index.next_candidate(buffer, behavior_ids)?;
        let (message, tried) = buffer.get_mut(message_idx)?;
        *tried = true;
        Some((arm_pos, message_idx, message.payload.to_shared()))
    }

    /// Selective receive is transactional: returning a candidate only marks it
    /// as tried. The message stays logically queued and capacity-accounted
    /// until `commit_receive_match` consumes the candidate whose pattern and
    /// guard actually succeeded.
    pub fn receive_match(&mut self, behavior_ids: &[u16]) -> Option<(usize, Arc<Vec<Value>>)> {
        // If the VM asks for another candidate before commit, the previous
        // candidate was rejected by its pattern/guard. It remains `tried` for
        // this receive expression but is no longer the commit target.
        self.active_match = None;
        self.ensure_receive_indexes();
        self.stage_arrivals();

        let indexes = self
            .receive_indexes
            .as_mut()
            .expect("selective receive indexes must be initialized");

        if let Some((pos, idx, payload)) = Self::scan_indexed(
            &mut self.system_skip_buffer,
            &mut indexes.system,
            behavior_ids,
        ) {
            self.active_match = Some((MatchLane::System, idx, Arc::clone(&payload)));
            return Some((pos, payload));
        }
        if let Some((pos, idx, payload)) = Self::scan_indexed(
            &mut self.local_skip_buffer,
            &mut indexes.local,
            behavior_ids,
        ) {
            self.active_match = Some((MatchLane::Local, idx, Arc::clone(&payload)));
            return Some((pos, payload));
        }
        if let Some((pos, idx, payload)) =
            Self::scan_indexed(&mut self.skip_buffer, &mut indexes.normal, behavior_ids)
        {
            self.active_match = Some((MatchLane::Normal, idx, Arc::clone(&payload)));
            return Some((pos, payload));
        }
        None
    }

    /// Total logical message count. Safe to query concurrently.
    pub fn len(&self) -> usize {
        self.queued_count.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True when a scheduler-local mailbox entry already carries this durable
    /// receiver-acceptance identity. Durable outbox delivery always uses the
    /// local lane, so checking the local live/staged queues is sufficient and
    /// does not disturb concurrent producer queues.
    pub fn contains_durable_id(&self, id: crate::runtime::persistence::DurableMessageId) -> bool {
        self.local_queue
            .iter()
            .any(|message| message.durable_id == Some(id))
            || self
                .local_skip_buffer
                .iter()
                .any(|(message, _)| message.durable_id == Some(id))
    }

    /// Snapshot the mailbox without changing logical ownership/counting.
    pub fn drain(&mut self) -> Vec<Message> {
        let mut snapshot = Vec::with_capacity(self.len());

        snapshot.extend(self.system_skip_buffer.iter().map(|(m, _)| m.clone()));
        let mut system_live = Vec::new();
        while let Some(msg) = self.system_queue.pop() {
            snapshot.push(msg.clone());
            system_live.push(msg);
        }
        for msg in system_live {
            self.system_queue.push(msg);
        }

        snapshot.extend(self.local_skip_buffer.iter().map(|(m, _)| m.clone()));
        snapshot.extend(self.local_queue.iter().cloned());

        snapshot.extend(self.skip_buffer.iter().map(|(m, _)| m.clone()));
        let mut normal_live = Vec::new();
        while let Some(msg) = self.normal_queue.pop() {
            snapshot.push(msg.clone());
            normal_live.push(msg);
        }
        for msg in normal_live {
            self.normal_queue.push(msg);
        }

        snapshot
    }

    /// Finish a scheduler turn with no active selective-receive transaction.
    /// Staged messages intentionally stay in their lane buffers: they are
    /// older than concurrently-arrived queue entries, so moving them to the
    /// back of a `SegQueue` would violate skipped-message FIFO ordering.
    pub fn flush_skip_buffer(&mut self) {
        self.reset_receive_match();
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn clear_tried_flags(&mut self) {
        for (_, tried) in self.system_skip_buffer.iter_mut() {
            *tried = false;
        }
        for (_, tried) in self.local_skip_buffer.iter_mut() {
            *tried = false;
        }
        for (_, tried) in self.skip_buffer.iter_mut() {
            *tried = false;
        }
        if let Some(indexes) = self.receive_indexes.as_mut() {
            indexes.system.reset_cursors();
            indexes.local.reset_cursors();
            indexes.normal.reset_cursors();
        }
    }

    fn invalidate_receive_indexes(&mut self) {
        if let Some(indexes) = self.receive_indexes.as_mut() {
            indexes.system.invalidate();
            indexes.local.invalidate();
            indexes.normal.invalidate();
        }
    }

    /// Commit exactly the most recently returned candidate and return its
    /// payload so the runtime can establish receiver-side ORCA ownership only
    /// after the pattern+guard succeeds.
    pub fn commit_receive_match(&mut self) -> Option<Arc<Vec<Value>>> {
        let (lane, idx, payload) = self.active_match.take()?;
        let _removed = match lane {
            MatchLane::System => self.system_skip_buffer.remove(idx),
            MatchLane::Local => self.local_skip_buffer.remove(idx),
            MatchLane::Normal => self.skip_buffer.remove(idx),
        }?;
        self.release_slot();
        self.invalidate_receive_indexes();
        self.clear_tried_flags();
        Some(payload)
    }

    /// Abort a selective-receive scan. No message is consumed and ownership
    /// state is unchanged; all rejected candidates become eligible for the
    /// next receive expression.
    pub fn reset_receive_match(&mut self) {
        self.active_match = None;
        self.clear_tried_flags();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(behavior_id: u16, sender: u64) -> Message {
        Message {
            behavior_id,
            payload: MessagePayload::from_slice(&[Value::int(42)]),
            sender,
            priority: MessagePriority::Normal,
            trace_id: None,
            durable_id: None,
        }
    }

    #[test]
    fn small_payloads_inline_and_large_payloads_spill() {
        let four = [Value::int(1), Value::int(2), Value::int(3), Value::int(4)];
        let inline = MessagePayload::from_slice(&four);
        assert!(inline.is_inline());
        assert_eq!(inline.as_slice(), &four);

        let five = [
            Value::int(1),
            Value::int(2),
            Value::int(3),
            Value::int(4),
            Value::int(5),
        ];
        let shared = MessagePayload::from_slice(&five);
        assert!(!shared.is_inline());
        assert_eq!(shared.as_slice(), &five);
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
        assert_eq!(popped.payload.as_slice(), &[Value::int(42)]);
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
                payload: MessagePayload::from_slice(&[Value::int(i)]),
                sender: i as u64,
                priority: MessagePriority::System,
                trace_id: None,
                durable_id: None,
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

#[cfg(test)]
mod transactional_receive_tests {
    use super::*;

    fn msg(behavior_id: u16, sender: u64, priority: MessagePriority) -> Message {
        Message {
            behavior_id,
            payload: MessagePayload::from_slice(&[Value::int(sender as i64)]),
            sender,
            priority,
            trace_id: None,
            durable_id: None,
        }
    }

    #[test]
    fn first_guard_rejection_then_second_commit_consumes_only_second() {
        let mut mb = Mailbox::new(8);
        mb.push(msg(7, 1, MessagePriority::Normal)).unwrap();
        mb.push(msg(7, 2, MessagePriority::Normal)).unwrap();

        let first = mb.receive_match(&[7]).expect("first candidate");
        assert_eq!(first.1[0].as_int(), Some(1));
        assert_eq!(mb.len(), 2, "candidate discovery must not consume");

        // Asking again models a failed guard on the first candidate.
        let second = mb.receive_match(&[7]).expect("second candidate");
        assert_eq!(second.1[0].as_int(), Some(2));
        let committed = mb.commit_receive_match().expect("commit second candidate");
        assert_eq!(committed[0].as_int(), Some(2));
        assert_eq!(mb.len(), 1);

        let remaining = mb.pop().expect("rejected candidate remains queued");
        assert_eq!(remaining.sender, 1);
        assert!(mb.is_empty());
    }

    #[test]
    fn reset_reexposes_rejected_candidate_without_consumption() {
        let mut mb = Mailbox::new(4);
        mb.push(msg(9, 11, MessagePriority::Normal)).unwrap();
        assert!(mb.receive_match(&[9]).is_some());
        assert_eq!(mb.len(), 1);
        mb.reset_receive_match();
        let again = mb.receive_match(&[9]).expect("candidate after reset");
        assert_eq!(again.1[0].as_int(), Some(11));
        assert_eq!(mb.len(), 1);
    }

    #[test]
    fn system_local_and_normal_candidates_share_commit_lifecycle() {
        for priority in [
            MessagePriority::System,
            MessagePriority::Normal,
            MessagePriority::Bulk,
        ] {
            let mut mb = Mailbox::new(4);
            let message = msg(3, priority as u64 + 20, priority);
            if priority == MessagePriority::Bulk {
                mb.push_local(message).unwrap();
            } else {
                mb.push(message).unwrap();
            }
            let (_, payload) = mb.receive_match(&[3]).expect("candidate");
            assert_eq!(mb.len(), 1, "candidate remains queued until commit");
            let committed = mb.commit_receive_match().expect("commit payload");
            assert_eq!(*committed, *payload);
            assert!(mb.is_empty());
        }
    }

    #[test]
    fn system_priority_precedes_normal_staged_candidates() {
        let mut mb = Mailbox::new(4);
        mb.push(msg(5, 1, MessagePriority::Normal)).unwrap();
        let first = mb.receive_match(&[5]).expect("normal candidate");
        assert_eq!(first.1[0].as_int(), Some(1));
        mb.reset_receive_match();

        mb.push(msg(5, 2, MessagePriority::System)).unwrap();
        let next = mb.receive_match(&[5]).expect("system candidate");
        assert_eq!(next.1[0].as_int(), Some(2));
    }

    #[test]
    fn turn_flush_preserves_skipped_fifo_before_new_arrivals() {
        let mut mb = Mailbox::new(8);
        mb.push(msg(4, 1, MessagePriority::Normal)).unwrap();
        mb.push(msg(4, 2, MessagePriority::Normal)).unwrap();
        assert!(mb.receive_match(&[4]).is_some());
        mb.flush_skip_buffer();
        mb.push(msg(4, 3, MessagePriority::Normal)).unwrap();

        assert_eq!(mb.pop().unwrap().sender, 1);
        assert_eq!(mb.pop().unwrap().sender, 2);
        assert_eq!(mb.pop().unwrap().sender, 3);
    }
    #[test]
    fn indexed_receive_preserves_fifo_across_arm_order() {
        let mut mb = Mailbox::new(8);
        mb.push_local(msg(20, 1, MessagePriority::Normal)).unwrap();
        mb.push_local(msg(10, 2, MessagePriority::Normal)).unwrap();

        let (arm, payload) = mb.receive_match(&[10, 20]).expect("candidate");
        assert_eq!(arm, 1, "oldest matching message wins before arm order");
        assert_eq!(payload[0].as_int(), Some(1));
    }

    #[test]
    fn indexed_receive_duplicate_behavior_uses_first_arm() {
        let mut mb = Mailbox::new(4);
        mb.push_local(msg(7, 11, MessagePriority::Normal)).unwrap();

        let (arm, payload) = mb.receive_match(&[7, 7]).expect("candidate");
        assert_eq!(arm, 0);
        assert_eq!(payload[0].as_int(), Some(11));
    }

    #[test]
    fn indexed_receive_rebuilds_after_middle_commit() {
        let mut mb = Mailbox::new(8);
        mb.push_local(msg(1, 1, MessagePriority::Normal)).unwrap();
        mb.push_local(msg(2, 2, MessagePriority::Normal)).unwrap();
        mb.push_local(msg(3, 3, MessagePriority::Normal)).unwrap();

        let first = mb.receive_match(&[2]).expect("middle candidate");
        assert_eq!(first.1[0].as_int(), Some(2));
        mb.commit_receive_match().expect("commit middle");

        let next = mb.receive_match(&[3]).expect("candidate after reindex");
        assert_eq!(next.1[0].as_int(), Some(3));
        mb.commit_receive_match().expect("commit tail");

        assert_eq!(mb.pop().unwrap().sender, 1);
        assert!(mb.is_empty());
    }

    #[test]
    fn indexed_receive_sees_arrival_after_initial_miss() {
        let mut mb = Mailbox::new(8);
        mb.push_local(msg(1, 1, MessagePriority::Normal)).unwrap();
        assert!(mb.receive_match(&[9]).is_none());

        mb.push_local(msg(9, 2, MessagePriority::Normal)).unwrap();
        let found = mb.receive_match(&[9]).expect("new indexed arrival");
        assert_eq!(found.1[0].as_int(), Some(2));
    }

    #[test]
    fn indexed_receive_reset_rewinds_behavior_cursor() {
        let mut mb = Mailbox::new(8);
        mb.push_local(msg(7, 11, MessagePriority::Normal)).unwrap();
        mb.push_local(msg(7, 22, MessagePriority::Normal)).unwrap();

        let first = mb.receive_match(&[7]).expect("first candidate");
        assert_eq!(first.1[0].as_int(), Some(11));

        let second = mb.receive_match(&[7]).expect("guard-retry candidate");
        assert_eq!(second.1[0].as_int(), Some(22));

        mb.reset_receive_match();

        let retried = mb.receive_match(&[7]).expect("candidate after reset");
        assert_eq!(
            retried.1[0].as_int(),
            Some(11),
            "reset must rewind indexed cursors and clear tried state"
        );
    }
}
