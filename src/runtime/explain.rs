//! Runtime-scoped EXPLAIN ANALYZE built from existing observable counters.
//!
//! This is an explicit, opt-in aggregate profile of a measured closure on
//! one runtime shard. It does not claim per-actor CPU attribution, wait-event
//! timing, allocation deltas, or durable I/O accounting. Those require
//! additional instrumentation and have separate overhead/correctness costs.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::{GcStats, MetricsSnapshot, Runtime, SchedulerStats};

pub const RUNTIME_EXPLAIN_SCHEMA_VERSION: u16 = 1;
pub const RUNTIME_EXPLAIN_MAILBOX_LIMIT: usize = 16;

/// Scheduler counter deltas observed during one measured scope.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExplainSchedulerActivity {
    pub tasks_processed: u64,
    pub tasks_local: u64,
    pub tasks_global: u64,
    pub tasks_stolen: u64,
    pub steal_attempts: u64,
    pub steal_successes: u64,
    pub empty_polls: u64,
}

impl ExplainSchedulerActivity {
    fn between(before: &SchedulerStats, after: &SchedulerStats) -> (Self, bool) {
        let mut reset_detected = false;
        let mut delta = |old: u64, new: u64| {
            if new < old {
                reset_detected = true;
            }
            new.saturating_sub(old)
        };
        let values = Self {
            tasks_processed: delta(before.total_tasks_processed, after.total_tasks_processed),
            tasks_local: delta(before.tasks_from_local_queue, after.tasks_from_local_queue),
            tasks_global: delta(before.tasks_from_global_queue, after.tasks_from_global_queue),
            tasks_stolen: delta(before.tasks_from_steal, after.tasks_from_steal),
            steal_attempts: delta(before.steal_attempts, after.steal_attempts),
            steal_successes: delta(before.steal_successes, after.steal_successes),
            empty_polls: delta(before.empty_polls, after.empty_polls),
        };
        (values, reset_detected)
    }
}

/// Mailbox pressure at the beginning and end of a measured scope.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExplainActorMailbox {
    pub actor_id: u64,
    pub pending_before: usize,
    pub pending_after: usize,
}

/// Read-only runtime-scope measurements produced by `Runtime::explain_analyze`.
///
/// All metrics are scoped to one `Runtime` instance (one shard). Scheduler
/// counters represent deltas unless reset during the measured closure. ORCA
/// counters are **snapshots of currently live actors**, not allocation deltas:
/// actor termination can remove previously counted allocations.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeExplainReport {
    pub schema_version: u16,
    /// Wall-clock time of the closure only, excluding before/after sampling.
    pub wall_time_ns: u64,
    pub scheduler: ExplainSchedulerActivity,
    /// True if a scheduler counter decreased while the closure executed.
    /// Deltas must be considered incomplete when this is true.
    pub scheduler_counter_reset: bool,
    pub actors_live_before: u64,
    pub actors_live_after: u64,
    pub mailbox_messages_before: usize,
    pub mailbox_messages_after: usize,
    pub dlq_depth_before: u64,
    pub dlq_depth_after: u64,
    /// Cumulative ORCA stats belonging to currently live actors at each end.
    pub gc_live_before: GcStats,
    pub gc_live_after: GcStats,
    /// Highest-pressure mailboxes, ordered by initial depth, then final depth.
    pub mailboxes: Vec<ExplainActorMailbox>,
    pub mailboxes_omitted: usize,
}

impl RuntimeExplainReport {
    fn between(before: MetricsSnapshot, after: MetricsSnapshot, elapsed: Duration) -> Self {
        let (scheduler, scheduler_counter_reset) =
            ExplainSchedulerActivity::between(&before.scheduler, &after.scheduler);
        let mut mailboxes = BTreeMap::<u64, (usize, usize)>::new();
        let mut mailbox_messages_before = 0usize;
        let mut mailbox_messages_after = 0usize;

        for item in &before.actors_mailboxes {
            mailbox_messages_before = mailbox_messages_before.saturating_add(item.depth);
            if item.depth > 0 {
                mailboxes.entry(item.actor_id).or_default().0 = item.depth;
            }
        }
        for item in &after.actors_mailboxes {
            mailbox_messages_after = mailbox_messages_after.saturating_add(item.depth);
            if item.depth > 0 {
                mailboxes.entry(item.actor_id).or_default().1 = item.depth;
            }
        }

        let mut mailboxes: Vec<_> = mailboxes
            .into_iter()
            .map(|(actor_id, (pending_before, pending_after))| ExplainActorMailbox {
                actor_id,
                pending_before,
                pending_after,
            })
            .collect();
        mailboxes.sort_by(|a, b| {
            b.pending_before
                .cmp(&a.pending_before)
                .then_with(|| b.pending_after.cmp(&a.pending_after))
                .then_with(|| a.actor_id.cmp(&b.actor_id))
        });
        let mailboxes_omitted = mailboxes.len().saturating_sub(RUNTIME_EXPLAIN_MAILBOX_LIMIT);
        mailboxes.truncate(RUNTIME_EXPLAIN_MAILBOX_LIMIT);

        Self {
            schema_version: RUNTIME_EXPLAIN_SCHEMA_VERSION,
            wall_time_ns: u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
            scheduler,
            scheduler_counter_reset,
            actors_live_before: before.actors_live,
            actors_live_after: after.actors_live,
            mailbox_messages_before,
            mailbox_messages_after,
            dlq_depth_before: before.dlq_depth,
            dlq_depth_after: after.dlq_depth,
            gc_live_before: before.gc,
            gc_live_after: after.gc,
            mailboxes,
            mailboxes_omitted,
        }
    }

    /// Compact human-readable diagnostic for terminal and development tooling.
    pub fn render_text(&self) -> String {
        let mut output = format!(
            "Nulang Runtime EXPLAIN ANALYZE (scope aggregate)\n  Elapsed: {:.3} ms\n  Scheduler tasks: {} (local {}, global {}, stolen {})\n  Empty polls: {}\n  Actors live: {} -> {}\n  Pending mailbox messages: {} -> {}\n  Dead-letter depth: {} -> {}\n",
            self.wall_time_ns as f64 / 1_000_000.0,
            self.scheduler.tasks_processed,
            self.scheduler.tasks_local,
            self.scheduler.tasks_global,
            self.scheduler.tasks_stolen,
            self.scheduler.empty_polls,
            self.actors_live_before,
            self.actors_live_after,
            self.mailbox_messages_before,
            self.mailbox_messages_after,
            self.dlq_depth_before,
            self.dlq_depth_after,
        );
        if self.scheduler_counter_reset {
            output.push_str("  Warning: scheduler counter reset detected; deltas are incomplete\n");
        }
        for mailbox in &self.mailboxes {
            output.push_str(&format!(
                "  Mailbox actor={} queued={} -> {}\n",
                mailbox.actor_id, mailbox.pending_before, mailbox.pending_after
            ));
        }
        if self.mailboxes_omitted > 0 {
            output.push_str(&format!(
                "  ... {} additional active mailboxes omitted\n",
                self.mailboxes_omitted
            ));
        }
        output
    }
}

impl Runtime {
    /// Measure a closure operating on this runtime, returning its original
    /// result alongside a structured, JSON-serializable execution report.
    ///
    /// The sampler does not dispatch actor work or reset existing counters.
    /// Both snapshots are taken outside the timed region, and the timer
    /// measures the closure's wall time (including anything it calls).
    ///
    /// This profile is shard-local and aggregate. It cannot infer per-actor
    /// CPU time, mailbox wait duration, or JIT/effect timing.
    ///
    /// `reset_scheduler_stats` must not be called inside `operation`; if
    /// it is, `scheduler_counter_reset` flags incomplete deltas.
    pub fn explain_analyze<R>(&mut self, operation: impl FnOnce(&mut Runtime) -> R) -> (R, RuntimeExplainReport) {
        let before = self.metrics_snapshot();
        let start = Instant::now();
        let value = operation(self);
        let elapsed = start.elapsed();
        let after = self.metrics_snapshot();
        (value, RuntimeExplainReport::between(before, after, elapsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_counter_regression_without_wrapping_deltas() {
        let before = SchedulerStats {
            total_tasks_processed: 8,
            tasks_from_local_queue: 5,
            ..SchedulerStats::default()
        };
        let after = SchedulerStats {
            total_tasks_processed: 1,
            tasks_from_local_queue: 0,
            ..SchedulerStats::default()
        };
        let (activity, reset) = ExplainSchedulerActivity::between(&before, &after);
        assert!(reset);
        assert_eq!(activity.tasks_processed, 0);
        assert_eq!(activity.tasks_local, 0);
    }
}
