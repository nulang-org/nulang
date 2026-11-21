use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Generation-based wakeup primitive for transport -> runtime admission.
///
/// A monotonically changing generation closes the classic lost-wakeup race:
/// callers snapshot the current generation before checking for work, then wait
/// only while that exact generation is still current. A notification that lands
/// between the work check and the wait is therefore observed immediately.
///
/// This primitive is intentionally transport-agnostic. TCP reader threads can
/// call `notify` after publishing packets, while the runtime can use
/// `wait_for_change` only when it has no immediately runnable work. Existing
/// nonblocking network polling semantics remain unchanged until that integration
/// is explicitly wired and measured.
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct NetworkWakeup {
    generation: Mutex<u64>,
    changed: Condvar,
}

#[allow(dead_code)]
impl NetworkWakeup {
    /// Snapshot the current notification generation.
    pub(crate) fn generation(&self) -> u64 {
        *self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Publish that network work may now be available and wake all sleepers.
    pub(crate) fn notify(&self) {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Wait until the notification generation differs from `observed` or the
    /// timeout expires. Returns `true` when a change was observed.
    ///
    /// The generation check and condition-variable wait share the same mutex,
    /// so a notify that races with admission cannot be lost.
    pub(crate) fn wait_for_change(&self, observed: u64, timeout: Duration) -> bool {
        let generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if *generation != observed {
            return true;
        }

        let (generation, _) = self
            .changed
            .wait_timeout_while(generation, timeout, |generation| *generation == observed)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *generation != observed
    }
}

#[cfg(test)]
mod tests {
    use super::NetworkWakeup;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn notification_before_wait_is_observed() {
        let wakeup = NetworkWakeup::default();
        let observed = wakeup.generation();
        wakeup.notify();

        assert!(wakeup.wait_for_change(observed, Duration::from_secs(1)));
    }

    #[test]
    fn waiter_unblocks_when_network_work_arrives() {
        let wakeup = Arc::new(NetworkWakeup::default());
        let observed = wakeup.generation();
        let rendezvous = Arc::new(Barrier::new(2));

        let waiter_wakeup = Arc::clone(&wakeup);
        let waiter_rendezvous = Arc::clone(&rendezvous);
        let waiter = thread::spawn(move || {
            waiter_rendezvous.wait();
            waiter_wakeup.wait_for_change(observed, Duration::from_secs(1))
        });

        rendezvous.wait();
        wakeup.notify();

        assert!(waiter.join().expect("waiter thread should not panic"));
    }

    #[test]
    fn timeout_without_notification_reports_no_change() {
        let wakeup = NetworkWakeup::default();
        let observed = wakeup.generation();

        assert!(!wakeup.wait_for_change(observed, Duration::from_millis(5)));
        assert_eq!(wakeup.generation(), observed);
    }
}
