//! Bounded, typed, per-runtime diagnostic event fanout.
//!
//! Inspired by Node.js diagnostics channels, but deliberately separated from
//! actor messaging, the durable journal, and authorization state. Observers
//! receive metadata only, never message payloads or authority tokens. A slow
//! observer drops telemetry rather than stalling an actor turn. This module
//! uses only `std` so minimal-feature and portable builds remain unaffected.

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// Increment when an event's meaning or fields change incompatibly.
pub const DIAGNOSTIC_SCHEMA_VERSION: u16 = 1;
/// Bound both the subscriber count and each subscriber's queued events.
pub const MAX_DIAGNOSTIC_SUBSCRIBERS: usize = 16;
pub const MAX_DIAGNOSTIC_BUFFER: usize = 1024;

/// Runtime-only metadata. Do not add arbitrary payloads, secrets, authority
/// grants, or process-global context to this API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticEvent {
    /// A message was dequeued for execution by an actor. This event does not
    /// attest to successful execution, journal admission, or persistence.
    ActorDispatch {
        actor_id: u64,
        behavior_id: u16,
        trace_id: u128,
        span_id: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSubscribeError {
    /// Buffer capacity must be between 1 and MAX_DIAGNOSTIC_BUFFER.
    InvalidCapacity,
    TooManySubscribers,
}

#[derive(Debug)]
struct Subscriber {
    id: u64,
    sender: SyncSender<DiagnosticEvent>,
}

#[derive(Debug, Default)]
struct Hub {
    subscribers: Mutex<Vec<Subscriber>>,
    active: AtomicUsize,
    next_id: AtomicU64,
    dropped: AtomicU64,
}

/// A diagnostic channel owned by exactly one runtime by default. Cloning
/// creates another handle to the *same* channel; distinct `new()` calls do
/// not share observers or telemetry.
#[derive(Debug, Clone, Default)]
pub struct RuntimeDiagnostics {
    hub: Arc<Hub>,
}

impl RuntimeDiagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe using a bounded per-observer buffer. No callbacks run on the
    /// scheduler thread. A dropped subscription unregisters immediately.
    pub fn subscribe(
        &self,
        capacity: usize,
    ) -> Result<DiagnosticSubscription, DiagnosticSubscribeError> {
        if !(1..=MAX_DIAGNOSTIC_BUFFER).contains(&capacity) {
            return Err(DiagnosticSubscribeError::InvalidCapacity);
        }
        let mut subscribers = self
            .hub
            .subscribers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if subscribers.len() >= MAX_DIAGNOSTIC_SUBSCRIBERS {
            return Err(DiagnosticSubscribeError::TooManySubscribers);
        }

        let (sender, receiver) = sync_channel(capacity);
        let id = self.hub.next_id.fetch_add(1, Ordering::Relaxed);
        subscribers.push(Subscriber { id, sender });
        self.hub.active.store(subscribers.len(), Ordering::Release);
        Ok(DiagnosticSubscription {
            id,
            receiver,
            hub: Arc::clone(&self.hub),
        })
    }

    /// Best-effort, nonblocking publication. No allocation, locking, or
    /// channel send occurs when there are no observers.
    #[inline]
    pub fn publish(&self, event: DiagnosticEvent) {
        if self.hub.active.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut subscribers = self
            .hub
            .subscribers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        subscribers.retain(|subscriber| match subscriber.sender.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.hub.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
        self.hub.active.store(subscribers.len(), Ordering::Release);
    }

    pub fn subscriber_count(&self) -> usize {
        self.hub.active.load(Ordering::Acquire)
    }

    /// Cumulative events discarded across full observer queues, counted once
    /// per slow subscriber. Diagnostic loss never changes program behavior.
    pub fn dropped_deliveries(&self) -> u64 {
        self.hub.dropped.load(Ordering::Relaxed)
    }
}

/// Owns the receiver and unregisters on drop. No global runtime state or
/// cross-runtime subscription is introduced by this observer handle.
pub struct DiagnosticSubscription {
    id: u64,
    receiver: Receiver<DiagnosticEvent>,
    hub: Arc<Hub>,
}

impl Deref for DiagnosticSubscription {
    type Target = Receiver<DiagnosticEvent>;

    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl Drop for DiagnosticSubscription {
    fn drop(&mut self) {
        let mut subscribers = self
            .hub
            .subscribers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        subscribers.retain(|subscriber| subscriber.id != self.id);
        self.hub.active.store(subscribers.len(), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT: DiagnosticEvent = DiagnosticEvent::ActorDispatch {
        actor_id: 7,
        behavior_id: 2,
        trace_id: 123,
        span_id: 456,
    };

    #[test]
    fn diagnostic_fanout_is_typed_and_runtime_isolated() {
        let runtime_a = RuntimeDiagnostics::new();
        let runtime_b = RuntimeDiagnostics::new();
        let first = runtime_a.subscribe(2).unwrap();
        let second = runtime_a.subscribe(2).unwrap();
        let isolated = runtime_b.subscribe(2).unwrap();

        runtime_a.publish(EVENT);
        assert_eq!(first.try_recv().unwrap(), EVENT);
        assert_eq!(second.try_recv().unwrap(), EVENT);
        assert!(isolated.try_recv().is_err());
    }

    #[test]
    fn observer_backpressure_drops_diagnostics_without_blocking() {
        let diagnostics = RuntimeDiagnostics::new();
        let slow = diagnostics.subscribe(1).unwrap();

        diagnostics.publish(EVENT);
        diagnostics.publish(EVENT);
        assert_eq!(diagnostics.dropped_deliveries(), 1);
        assert_eq!(slow.try_recv().unwrap(), EVENT);
        assert!(slow.try_recv().is_err());

        diagnostics.publish(EVENT);
        assert_eq!(slow.try_recv().unwrap(), EVENT);
    }

    #[test]
    fn subscriptions_are_removed_immediately_when_dropped() {
        let diagnostics = RuntimeDiagnostics::new();
        let observer = diagnostics.subscribe(1).unwrap();
        assert_eq!(diagnostics.subscriber_count(), 1);
        drop(observer);
        assert_eq!(diagnostics.subscriber_count(), 0);
        diagnostics.publish(EVENT);
        assert_eq!(diagnostics.dropped_deliveries(), 0);
    }

    #[test]
    fn subscriber_and_queue_limits_are_fail_closed() {
        let diagnostics = RuntimeDiagnostics::new();
        assert!(matches!(
            diagnostics.subscribe(0),
            Err(DiagnosticSubscribeError::InvalidCapacity)
        ));
        assert!(matches!(
            diagnostics.subscribe(MAX_DIAGNOSTIC_BUFFER + 1),
            Err(DiagnosticSubscribeError::InvalidCapacity)
        ));
        let observers: Vec<_> = (0..MAX_DIAGNOSTIC_SUBSCRIBERS)
            .map(|_| diagnostics.subscribe(1).unwrap())
            .collect();
        assert!(matches!(
            diagnostics.subscribe(1),
            Err(DiagnosticSubscribeError::TooManySubscribers)
        ));
        drop(observers);
        assert_eq!(diagnostics.subscriber_count(), 0);
        assert!(diagnostics.subscribe(1).is_ok());
    }
}
