//! Hierarchical timer wheel for actor runtime.
//!
//! Implements BEAM-style timer primitives:
//! - `send_after`: Schedule a message to an actor after a delay
//! - `exit_after`: Schedule an actor exit after a delay
//! - `kill_after`: Schedule an unconditional kill after a delay
//! - `cancel_timer`: Cancel a scheduled timer
//! - `read_timer`: Get remaining time for a timer

use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::runtime::WorkflowOperationId;
use crate::vm::Value;

/// Unique identifier for a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u64);

/// What action to take when a timer fires.
#[derive(Debug, Clone, PartialEq)]
pub enum TimerMessage {
    /// Send a behavior message to the target actor.
    Send {
        behavior_id: u16,
        payload: Vec<Value>,
    },
    /// Send a behavior message with an opaque context string (used by durable
    /// workflow timers to carry the timer name through to the fire handler).
    SendWithContext {
        behavior_id: u16,
        payload: Vec<Value>,
        context: String,
        /// Stable workflow operation identity carried from durable TimerSet
        /// through live delivery and recovery re-arm. Generic actor timers use
        /// `None`.
        operation_id: Option<WorkflowOperationId>,
    },
    /// Exit the target actor with a reason.
    Exit { reason: String },
    /// Unconditionally kill the target actor.
    Kill,
    /// Wake an actor suspended in a timed selective receive
    /// (`receive { ... } after ms => body`): the runtime marks the wait as
    /// timed out and resumes the suspended behavior, which then runs the
    /// after body.
    ReceiveWaitTimeout,
    /// Retry an LLM call for the target actor. The runtime re-dispatches
    /// the in-flight request on fire.
    LlmRetry,
    /// Wake an actor suspended by `Timer.sleep(ms)`. The runtime resumes
    /// the suspended behavior, which re-executes the PerformAsync yielding Ready.
    TimerSleepWake,
}

// -- Virtual clock for deterministic testing ---------------------------

/// A manually advanced clock that replaces `Instant::now()` during tests.
///
/// When installed on a `Runtime`, all timer scheduling, firing, and
/// deadline calculations use this clock instead of wall time, enabling
/// deterministic, sub-second workflow tests.
#[derive(Debug, Clone)]
pub struct VirtualClock {
    /// Wall-clock time captured when the virtual clock was created.
    base: Instant,
    /// How far the virtual clock has been advanced beyond `base`.
    elapsed: Duration,
}

impl VirtualClock {
    /// Create a new virtual clock frozen at the current wall time.
    pub fn new() -> Self {
        VirtualClock {
            base: Instant::now(),
            elapsed: Duration::ZERO,
        }
    }

    /// The current virtual time.
    pub fn now(&self) -> Instant {
        self.base + self.elapsed
    }

    /// Advance the virtual clock by `duration`.
    pub fn advance(&mut self, duration: Duration) {
        self.elapsed += duration;
    }

    /// Total elapsed virtual time since creation.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

impl Default for VirtualClock {
    fn default() -> Self {
        Self::new()
    }
}

/// A single timer entry in the wheel.
#[derive(Debug)]
pub struct TimerEntry {
    pub id: TimerId,
    pub target_actor: u64,
    pub message: TimerMessage,
    pub fire_at: Instant,
    pub cancelled: AtomicBool,
}

// Ord for BinaryHeap: soonest fire_at first
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.id.0 == other.id.0
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse: BinaryHeap is max-heap, we want min-heap (soonest first)
        other.fire_at.cmp(&self.fire_at)
    }
}

/// Hierarchical timer wheel for the actor runtime.
///
/// Uses a min-heap ordered by fire time. Timers are evaluated lazily
/// on each `tick` call.
///
/// # Example
/// ```
/// use nulang::runtime::TimerWheel;
/// use std::time::Duration;
///
/// let wheel = TimerWheel::new();
/// let timer_id = wheel.send_after(Duration::from_secs(1), 42, 1, vec![]);
/// assert!(!wheel.is_empty());
/// ```
pub struct TimerWheel {
    next_id: AtomicU64,
    timers: RwLock<BinaryHeap<TimerEntry>>,
}

impl TimerWheel {
    /// Create a new, empty timer wheel.
    pub fn new() -> Self {
        TimerWheel {
            next_id: AtomicU64::new(1),
            timers: RwLock::new(BinaryHeap::new()),
        }
    }

    /// Schedule a message to be sent to an actor after a delay.
    ///
    /// Returns the `TimerId` which can be used to cancel the timer.
    pub fn send_after(
        &self,
        delay: Duration,
        target_actor: u64,
        behavior_id: u16,
        payload: Vec<Value>,
    ) -> TimerId {
        self.send_after_with_context(delay, target_actor, behavior_id, payload, String::new())
    }

    /// Schedule a message to be sent to an actor after a delay, carrying an
    /// opaque context string through to the fire handler.
    ///
    /// Returns the `TimerId` which can be used to cancel the timer.
    pub fn send_after_with_context(
        &self,
        delay: Duration,
        target_actor: u64,
        behavior_id: u16,
        payload: Vec<Value>,
        context: String,
    ) -> TimerId {
        self.send_after_with_operation_context(
            delay,
            target_actor,
            behavior_id,
            payload,
            context,
            None,
        )
    }

    /// Schedule a workflow timer while preserving its replay-stable operation
    /// identity through the in-memory timer wheel.
    pub fn send_after_with_operation_context(
        &self,
        delay: Duration,
        target_actor: u64,
        behavior_id: u16,
        payload: Vec<Value>,
        context: String,
        operation_id: Option<WorkflowOperationId>,
    ) -> TimerId {
        let id = TimerId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let fire_at = Instant::now() + delay;

        let entry = TimerEntry {
            id,
            target_actor,
            message: TimerMessage::SendWithContext {
                behavior_id,
                payload,
                context,
                operation_id,
            },
            fire_at,
            cancelled: AtomicBool::new(false),
        };

        if let Ok(mut timers) = self.timers.write() {
            timers.push(entry);
        }

        id
    }

    /// Schedule an actor exit after a delay.
    pub fn exit_after(&self, delay: Duration, target_actor: u64, reason: String) -> TimerId {
        let id = TimerId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let fire_at = Instant::now() + delay;

        let entry = TimerEntry {
            id,
            target_actor,
            message: TimerMessage::Exit { reason },
            fire_at,
            cancelled: AtomicBool::new(false),
        };

        if let Ok(mut timers) = self.timers.write() {
            timers.push(entry);
        }

        id
    }

    /// Schedule an unconditional kill after a delay.
    pub fn kill_after(&self, delay: Duration, target_actor: u64) -> TimerId {
        let id = TimerId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let fire_at = Instant::now() + delay;

        let entry = TimerEntry {
            id,
            target_actor,
            message: TimerMessage::Kill,
            fire_at,
            cancelled: AtomicBool::new(false),
        };

        if let Ok(mut timers) = self.timers.write() {
            timers.push(entry);
        }

        id
    }

    /// Schedule a receive-wait timeout for an actor suspended in a timed
    /// selective receive. On fire the runtime marks the actor's wait as
    /// timed out and resumes it; the re-executed `ReceiveWait` then resolves
    /// with the no-match sentinel.
    pub fn receive_wait_timeout(&self, delay: Duration, target_actor: u64) -> TimerId {
        let id = TimerId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let fire_at = Instant::now() + delay;

        let entry = TimerEntry {
            id,
            target_actor,
            message: TimerMessage::ReceiveWaitTimeout,
            fire_at,
            cancelled: AtomicBool::new(false),
        };

        if let Ok(mut timers) = self.timers.write() {
            timers.push(entry);
        }

        id
    }

    /// Schedule a `TimerSleepWake` message to fire after `delay`, resuming
    /// an actor suspended on `Timer.sleep`.
    pub fn timer_sleep_wake(&self, delay: Duration, target_actor: u64) -> TimerId {
        let id = TimerId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let fire_at = Instant::now() + delay;
        let entry = TimerEntry {
            id,
            target_actor,
            message: TimerMessage::TimerSleepWake,
            fire_at,
            cancelled: AtomicBool::new(false),
        };
        if let Ok(mut timers) = self.timers.write() {
            timers.push(entry);
        }
        id
    }

    /// Schedule an LLM retry for an actor after a delay. On fire the
    /// runtime re-builds and re-dispatches the in-flight request.
    pub fn schedule_llm_retry(&self, delay: Duration, target_actor: u64) -> TimerId {
        let id = TimerId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let fire_at = Instant::now() + delay;

        let entry = TimerEntry {
            id,
            target_actor,
            message: TimerMessage::LlmRetry,
            fire_at,
            cancelled: AtomicBool::new(false),
        };

        if let Ok(mut timers) = self.timers.write() {
            timers.push(entry);
        }

        id
    }

    /// Cancel a timer by its id.
    ///
    /// Returns true if the timer was found and cancelled.
    /// Cancellation is lazy: the timer remains in the heap until
    /// `tick` removes it.
    pub fn cancel(&self, timer_id: TimerId) -> bool {
        let timers = match self.timers.read() {
            Ok(t) => t,
            Err(_) => return false,
        };

        for entry in timers.iter() {
            if entry.id == timer_id {
                entry.cancelled.store(true, Ordering::SeqCst);
                return true;
            }
        }

        false
    }

    /// Get the remaining time for a timer.
    ///
    /// Returns `None` if the timer is not found or has already fired.
    pub fn remaining(&self, timer_id: TimerId) -> Option<Duration> {
        let timers = match self.timers.read() {
            Ok(t) => t,
            Err(_) => return None,
        };

        for entry in timers.iter() {
            if entry.id == timer_id && !entry.cancelled.load(Ordering::SeqCst) {
                let now = Instant::now();
                if entry.fire_at > now {
                    return Some(entry.fire_at - now);
                }
                return Some(Duration::ZERO);
            }
        }

        None
    }

    /// Tick the timer wheel: collect all timers that have fired.
    ///
    /// Returns a list of `(target_actor, message)` pairs for timers
    /// whose fire time has passed. The caller is responsible for
    /// delivering these messages.
    pub fn tick(&self, now: Instant) -> Vec<(u64, TimerMessage)> {
        let mut fired = Vec::new();

        if let Ok(mut timers) = self.timers.write() {
            while let Some(entry) = timers.peek() {
                if entry.cancelled.load(Ordering::SeqCst) {
                    timers.pop();
                    continue;
                }
                if entry.fire_at <= now {
                    if let Some(entry) = timers.pop() {
                        fired.push((entry.target_actor, entry.message));
                    }
                } else {
                    break;
                }
            }
        }

        fired
    }

    /// Earliest fire time among active (non-cancelled) timers.
    ///
    /// Returns `None` when no active timer is scheduled. O(n) over the
    /// wheel; intended for the scheduler's drain path, not the hot loop.
    pub fn next_deadline(&self) -> Option<Instant> {
        let timers = match self.timers.read() {
            Ok(t) => t,
            Err(_) => return None,
        };
        timers
            .iter()
            .filter(|e| !e.cancelled.load(Ordering::SeqCst))
            .map(|e| e.fire_at)
            .min()
    }

    /// Count of active (non-cancelled) timers.
    pub fn len(&self) -> usize {
        let timers = match self.timers.read() {
            Ok(t) => t,
            Err(_) => return 0,
        };

        timers
            .iter()
            .filter(|e| !e.cancelled.load(Ordering::SeqCst))
            .count()
    }

    /// True if no active timers.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for TimerWheel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let wheel = TimerWheel::new();
        assert!(wheel.is_empty());
        assert_eq!(wheel.len(), 0);
    }

    #[test]
    fn test_send_after() {
        let wheel = TimerWheel::new();
        let id = wheel.send_after(Duration::from_millis(100), 42, 1, vec![Value::int(123)]);

        assert!(!wheel.is_empty());
        assert!(wheel.remaining(id).is_some());
    }

    #[test]
    fn test_cancel() {
        let wheel = TimerWheel::new();
        let id = wheel.send_after(Duration::from_secs(10), 42, 1, vec![]);

        assert!(wheel.cancel(id));
        assert!(wheel.is_empty());
    }

    #[test]
    fn test_tick_fires_overdue() {
        let wheel = TimerWheel::new();

        // Create a timer that should fire immediately (0ms delay)
        let _id = wheel.send_after(Duration::from_nanos(1), 42, 1, vec![Value::int(99)]);

        // Small sleep to let time pass
        std::thread::sleep(Duration::from_millis(10));

        let fired = wheel.tick(Instant::now());
        assert!(!fired.is_empty(), "Timer should have fired");
        assert_eq!(fired[0].0, 42);
    }

    #[test]
    fn test_exit_after() {
        let wheel = TimerWheel::new();
        let _id = wheel.exit_after(Duration::from_secs(1), 42, "shutdown".to_string());

        assert_eq!(wheel.len(), 1);
    }

    #[test]
    fn test_kill_after() {
        let wheel = TimerWheel::new();
        let _id = wheel.kill_after(Duration::from_secs(1), 42);

        assert_eq!(wheel.len(), 1);
    }

    #[test]
    fn test_receive_wait_timeout() {
        let wheel = TimerWheel::new();
        let id = wheel.receive_wait_timeout(Duration::from_millis(50), 42);

        assert_eq!(wheel.len(), 1);
        let fired = wheel.tick(Instant::now() + Duration::from_secs(1));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, 42);
        assert_eq!(fired[0].1, TimerMessage::ReceiveWaitTimeout);
        assert!(wheel.is_empty());
        assert!(wheel.remaining(id).is_none());
    }

    #[test]
    fn test_virtual_clock_advance_fires_timer() {
        let mut clock = VirtualClock::new();
        let start = clock.now();

        // Nothing fired at t=0
        let wheel = TimerWheel::new();
        let _id = wheel.send_after(Duration::from_secs(5), 42, 1, vec![Value::int(1)]);
        assert_eq!(wheel.tick(clock.now()).len(), 0);

        // Advance 3s — still not enough
        clock.advance(Duration::from_secs(3));
        assert_eq!(wheel.tick(clock.now()).len(), 0);

        // Advance 3 more seconds — now 6s elapsed, timer at 5s fires
        clock.advance(Duration::from_secs(3));
        let fired = wheel.tick(clock.now());
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, 42);

        // Virtual clock preserves its base
        assert!(clock.now() > start);
        assert!(clock.elapsed() >= Duration::from_secs(6));
    }

    #[test]
    fn test_virtual_clock_advance_accumulates() {
        let mut clock = VirtualClock::new();
        clock.advance(Duration::from_millis(100));
        clock.advance(Duration::from_millis(200));
        assert!(clock.elapsed() >= Duration::from_millis(300));
    }
}
