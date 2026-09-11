#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))


def replace_between(path: str, start: str, end: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    if text.count(start) != 1 or text.count(end) != 1:
        raise SystemExit(f"{path}: non-unique replacement boundary")
    i = text.index(start)
    j = text.index(end, i)
    p.write_text(text[:i] + new + text[j:])


mailbox_impl = r'''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchLane {
    System,
    Local,
    Normal,
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
    /// The most recently returned candidate. A second `receive_match` call
    /// means the previous candidate's guard rejected it; only this active
    /// candidate may be consumed by `commit_receive_match`.
    active_match: Option<(MatchLane, usize)>,
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
            active_match: None,
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
            self.release_slot();
        }
        result
    }

    fn stage_arrivals(&mut self) {
        // Scheduler-local system messages join the system lane; other local
        // traffic stays in its own lane so its FIFO position is stable.
        while let Some(msg) = self.local_queue.pop_front() {
            if msg.priority == MessagePriority::System {
                self.system_skip_buffer.push_back((msg, false));
            } else {
                self.local_skip_buffer.push_back((msg, false));
            }
        }
        while let Some(msg) = self.system_queue.pop() {
            self.system_skip_buffer.push_back((msg, false));
        }
        while let Some(msg) = self.normal_queue.pop() {
            self.skip_buffer.push_back((msg, false));
        }
    }

    fn scan_staged(
        buffer: &mut VecDeque<(Message, bool)>,
        behavior_ids: &[u16],
    ) -> Option<(usize, usize, Arc<Vec<Value>>)> {
        for (idx, (msg, tried)) in buffer.iter_mut().enumerate() {
            if *tried {
                continue;
            }
            if let Some(pos) = behavior_ids.iter().position(|&id| id == msg.behavior_id) {
                *tried = true;
                return Some((pos, idx, Arc::clone(&msg.payload)));
            }
        }
        None
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
        self.stage_arrivals();

        if let Some((pos, idx, payload)) =
            Self::scan_staged(&mut self.system_skip_buffer, behavior_ids)
        {
            self.active_match = Some((MatchLane::System, idx));
            return Some((pos, payload));
        }
        if let Some((pos, idx, payload)) =
            Self::scan_staged(&mut self.local_skip_buffer, behavior_ids)
        {
            self.active_match = Some((MatchLane::Local, idx));
            return Some((pos, payload));
        }
        if let Some((pos, idx, payload)) = Self::scan_staged(&mut self.skip_buffer, behavior_ids) {
            self.active_match = Some((MatchLane::Normal, idx));
            return Some((pos, payload));
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
    }

    /// Commit exactly the most recently returned candidate and return its
    /// payload so the runtime can establish receiver-side ORCA ownership only
    /// after the pattern+guard succeeds.
    pub fn commit_receive_match(&mut self) -> Option<Arc<Vec<Value>>> {
        let (lane, idx) = self.active_match.take()?;
        let removed = match lane {
            MatchLane::System => self.system_skip_buffer.remove(idx),
            MatchLane::Local => self.local_skip_buffer.remove(idx),
            MatchLane::Normal => self.skip_buffer.remove(idx),
        }?;
        self.release_slot();
        self.clear_tried_flags();
        Some(removed.0.payload)
    }

    /// Abort a selective-receive scan. No message is consumed and ownership
    /// state is unchanged; all rejected candidates become eligible for the
    /// next receive expression.
    pub fn reset_receive_match(&mut self) {
        self.active_match = None;
        self.clear_tried_flags();
    }
}

'''

replace_between(
    "src/runtime/mailbox.rs",
    "#[repr(align(64))]\npub struct Mailbox",
    "#[cfg(test)]\nmod tests",
    mailbox_impl,
)

transaction_tests = r'''

#[cfg(test)]
mod transactional_receive_tests {
    use super::*;

    fn msg(behavior_id: u16, sender: u64, priority: MessagePriority) -> Message {
        Message {
            behavior_id,
            payload: Arc::new(vec![Value::int(sender as i64)]),
            sender,
            priority,
            trace_id: None,
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
}
'''

mailbox = Path("src/runtime/mailbox.rs")
text = mailbox.read_text()
if "mod transactional_receive_tests" in text:
    raise SystemExit("src/runtime/mailbox.rs: transactional tests already present")
mailbox.write_text(text + transaction_tests)

replace_once(
    "src/runtime/callbacks.rs",
    '''        // ORCA receiver protocol: hold heap pointers carried by the message.\n        rt.hold_payload_refs(actor_id, &*payload);\n        Some((\n            pos,\n            Arc::try_unwrap(payload).unwrap_or_else(|arc| (*arc).clone()),\n        ))\n    }\n\n    fn commit_receive_match(&mut self) {\n        let mut rt = self.runtime.borrow_mut();\n        if let Some(actor_id) = rt.current_actor {\n            if let Some(actor) = rt.actors.get_mut(&actor_id) {\n                actor.mailbox.commit_receive_match();\n            }\n        }\n    }\n'''.replace('\\n', '\n'),
    '''        // Ownership is established only after pattern+guard commit. Until\n        // then the message remains logically queued and its in-flight ORCA\n        // reference keeps the payload alive.\n        Some((\n            pos,\n            Arc::try_unwrap(payload).unwrap_or_else(|arc| (*arc).clone()),\n        ))\n    }\n\n    fn commit_receive_match(&mut self) {\n        let mut rt = self.runtime.borrow_mut();\n        if let Some(actor_id) = rt.current_actor {\n            let payload = rt\n                .actors\n                .get_mut(&actor_id)\n                .and_then(|actor| actor.mailbox.commit_receive_match());\n            if let Some(payload) = payload {\n                rt.hold_payload_refs(actor_id, &payload);\n            }\n        }\n    }\n'''.replace('\\n', '\n'),
)

replace_once(
    "src/runtime/callbacks.rs",
    '''            // ORCA receiver protocol: hold heap pointers carried by the message.\n            (*self.runtime).hold_payload_refs(self.actor_id, &*payload);\n            Some((\n                pos,\n                Arc::try_unwrap(payload).unwrap_or_else(|arc| (*arc).clone()),\n            ))\n        }\n    }\n'''.replace('\\n', '\n'),
    '''            // Ownership is established only by commit_receive_match after\n            // the pattern+guard succeeds.\n            Some((\n                pos,\n                Arc::try_unwrap(payload).unwrap_or_else(|arc| (*arc).clone()),\n            ))\n        }\n    }\n'''.replace('\\n', '\n'),
)

replace_once(
    "src/runtime/callbacks.rs",
    '''    fn commit_receive_match(&mut self) {\n        unsafe {\n            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {\n                actor.mailbox.commit_receive_match();\n            }\n        }\n    }\n'''.replace('\\n', '\n'),
    '''    fn commit_receive_match(&mut self) {\n        unsafe {\n            let payload = (*self.runtime)\n                .actors\n                .get_mut(&self.actor_id)\n                .and_then(|actor| actor.mailbox.commit_receive_match());\n            if let Some(payload) = payload {\n                (*self.runtime).hold_payload_refs(self.actor_id, &payload);\n            }\n        }\n    }\n'''.replace('\\n', '\n'),
)

print("transactional selective receive staged; ORCA hold moved to commit")
