//! Best-effort, one-shot request/reply over process-local cancelable aliases.
//!
//! This is a typed Rust embedding API, not a language effect or durable wire
//! protocol. The owning Runtime serializes cancellation, expiry and admission.

use super::{ActorAlias, AliasSendStatus, Runtime};
use crate::vm::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A process-local correlation reference that round-trips through a signed
/// Nulang tagged Int. References never repeat within a process lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalRequestRef(u64);

impl LocalRequestRef {
    pub fn as_int(self) -> i64 {
        self.0 as i64
    }
}

/// Address given to a trusted local reply producer. The reference is placed
/// in the first element of every accepted reply payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalReplyTicket {
    pub(crate) reference: LocalRequestRef,
    pub(crate) alias: ActorAlias,
}

impl LocalReplyTicket {
    pub fn reference(self) -> LocalRequestRef {
        self.reference
    }

    pub fn alias(self) -> ActorAlias {
        self.alias
    }
}

/// Admission result for a one-shot reply attempt. Accepted means queued, not
/// processed; retry on Backpressured, never retry Expired or Inactive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalReplyStatus {
    Accepted,
    Backpressured,
    Expired,
    Inactive,
}

#[derive(Debug, Clone, Copy)]
struct PendingLocalRequest {
    owner: u64,
    alias: ActorAlias,
    deadline: Instant,
}

/// Private runtime-scoped state for outstanding one-shot reply tickets.
#[derive(Default)]
pub(crate) struct LocalReplyRegistry {
    pending: HashMap<LocalRequestRef, PendingLocalRequest>,
    by_owner: HashMap<u64, HashSet<LocalRequestRef>>,
}

static NEXT_LOCAL_REQUEST_REF: AtomicU64 = AtomicU64::new(1);
/// Keep the opaque request reference positive when encoded as tagged Int.
const MAX_LOCAL_REQUEST_REF: u64 = (1 << 46) - 1;

impl Runtime {
    /// Start a local, one-shot request/reply conversation addressed to owner.
    ///
    /// The expiration deadline is an absolute monotonic Instant. A request
    /// never outlives its owner; invalid/expired deadlines, dead owners,
    /// exhausted tokens and alias capacity limits all return None.
    pub fn begin_local_reply_request(
        &mut self,
        owner: u64,
        deadline: Instant,
    ) -> Option<LocalReplyTicket> {
        if deadline <= Instant::now() {
            return None;
        }
        let token = NEXT_LOCAL_REQUEST_REF
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current > 0 && current <= MAX_LOCAL_REQUEST_REF)
                    .then_some(current + 1)
            })
            .ok()?;
        let alias = self.create_local_alias(owner)?;
        let reference = LocalRequestRef(token);
        self.local_reply_requests.pending.insert(
            reference,
            PendingLocalRequest {
                owner,
                alias,
                deadline,
            },
        );
        self.local_reply_requests
            .by_owner
            .entry(owner)
            .or_default()
            .insert(reference);
        Some(LocalReplyTicket { reference, alias })
    }

    /// Cancel exactly one request that belongs to owner; accepted replies
    /// already in the mailbox are not retracted.
    pub fn cancel_local_reply_request(
        &mut self,
        owner: u64,
        reference: LocalRequestRef,
    ) -> bool {
        if self
            .local_reply_requests
            .pending
            .get(&reference)
            .is_none_or(|pending| pending.owner != owner)
        {
            return false;
        }
        self.retire_local_reply_request(reference);
        true
    }

    /// Queue at most one reply, prefixed with its correlation reference.
    /// This checks the real monotonic clock on every attempt. No timer is
    /// scheduled: expire_local_reply_requests is the optional eager sweeper.
    pub fn send_local_reply(
        &mut self,
        ticket: LocalReplyTicket,
        behavior_id: u16,
        values: &[Value],
    ) -> LocalReplyStatus {
        self.send_local_reply_at(ticket, behavior_id, values, Instant::now())
    }

    /// Testable internal clock seam; production callers use send_local_reply.
    pub(crate) fn send_local_reply_at(
        &mut self,
        ticket: LocalReplyTicket,
        behavior_id: u16,
        values: &[Value],
        now: Instant,
    ) -> LocalReplyStatus {
        let Some(pending) = self.local_reply_requests.pending.get(&ticket.reference).copied() else {
            return LocalReplyStatus::Inactive;
        };
        if pending.alias != ticket.alias {
            return LocalReplyStatus::Inactive;
        }
        if now >= pending.deadline {
            self.retire_local_reply_request(ticket.reference);
            return LocalReplyStatus::Expired;
        }

        // Each accepted reply carries its own reference as the FIRST
        // argument. This is a conventional envelope for a dedicated reply
        // behavior whose first parameter must be an Int reference.
        let mut payload = Vec::with_capacity(values.len() + 1);
        payload.push(Value::int(ticket.reference.as_int()));
        payload.extend_from_slice(values);
        match self.send_to_local_alias(ticket.alias, behavior_id, &payload) {
            AliasSendStatus::Accepted => {
                // Admission and alias revocation run under the same Runtime
                // mutable borrow: the next reply cannot race into this alias.
                self.retire_local_reply_request(ticket.reference);
                LocalReplyStatus::Accepted
            }
            AliasSendStatus::Backpressured => LocalReplyStatus::Backpressured,
            AliasSendStatus::Inactive => {
                self.retire_local_reply_request(ticket.reference);
                LocalReplyStatus::Inactive
            }
        }
    }

    /// Eagerly retire deadlines that are no longer valid. This is explicit:
    /// no scheduler callback or background task is installed by this API.
    pub fn expire_local_reply_requests(&mut self, now: Instant) -> usize {
        let expired: Vec<_> = self
            .local_reply_requests
            .pending
            .iter()
            .filter(|(_, state)| now >= state.deadline)
            .map(|(reference, _)| *reference)
            .collect();
        let count = expired.len();
        for reference in expired {
            self.retire_local_reply_request(reference);
        }
        count
    }

    /// Safe, payload-free metric for operational introspection.
    pub fn local_reply_request_count(&self) -> usize {
        self.local_reply_requests.pending.len()
    }

    fn retire_local_reply_request(&mut self, reference: LocalRequestRef) {
        let Some(pending) = self.local_reply_requests.pending.remove(&reference) else {
            return;
        };
        if let Some(references) = self.local_reply_requests.by_owner.get_mut(&pending.owner) {
            references.remove(&reference);
            if references.is_empty() {
                self.local_reply_requests.by_owner.remove(&pending.owner);
            }
        }
        self.deactivate_local_alias(pending.owner, pending.alias);
    }

    /// Reap all pending local replies owned by a terminated actor, without
    /// leaving a request registry that grows across successive actor exits.
    pub(crate) fn retire_local_reply_requests_of_owner(&mut self, owner: u64) {
        let Some(references) = self.local_reply_requests.by_owner.remove(&owner) else {
            return;
        };
        for reference in references {
            if let Some(pending) = self.local_reply_requests.pending.remove(&reference) {
                self.deactivate_local_alias(owner, pending.alias);
            }
        }
    }
}
