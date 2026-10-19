//! Runtime-scoped EXPLAIN ANALYZE contract tests.
//!
//! Measurements are shard-local aggregate counters over a measured closure,
//! not invented per-actor CPU/wait timings.

use nulang::runtime::{Actor, Runtime};
use nulang::vm::Value;

fn noop(_actor: &mut Actor, _args: &[Value]) {}

fn spawn_handle_actor(runtime: &mut Runtime) -> u64 {
    let id = runtime.spawn_actor(Box::new(|| vec![]));
    runtime
        .actors
        .get_mut(&id)
        .expect("new actor must exist")
        .register_behavior("handle", noop);
    id
}

#[test]
fn measured_scope_reports_real_scheduler_delta_and_queued_mailboxes() {
    let mut runtime = Runtime::new();
    let actor = spawn_handle_actor(&mut runtime);
    runtime.send_message(actor, "handle", &[Value::int(9)]);

    let (answer, report) = runtime.explain_analyze(|rt| {
        rt.run_scheduler();
        42
    });

    assert_eq!(answer, 42);
    assert_eq!(report.schema_version, 1);
    assert_eq!(report.scheduler.tasks_processed, 1);
    assert_eq!(report.mailbox_messages_before, 1);
    assert_eq!(report.mailbox_messages_after, 0);
    assert_eq!(report.actors_live_before, report.actors_live_after);
    assert!(!report.scheduler_counter_reset);
    assert!(report.render_text().contains("Runtime EXPLAIN ANALYZE"));

    let serialized = serde_json::to_value(&report).unwrap();
    assert_eq!(serialized["schema_version"], 1);
    assert_eq!(serialized["scheduler"]["tasks_processed"], 1);
}

#[test]
fn a_second_measured_scope_does_not_reuse_previous_counters() {
    let mut runtime = Runtime::new();
    let actor = spawn_handle_actor(&mut runtime);
    runtime.send_message(actor, "handle", &[]);
    let (_, first) = runtime.explain_analyze(|rt| rt.run_scheduler());
    let (_, second) = runtime.explain_analyze(|_| ());
    assert_eq!(first.scheduler.tasks_processed, 1);
    assert_eq!(second.scheduler.tasks_processed, 0);
    assert_eq!(second.scheduler.empty_polls, 0);
    assert_eq!(second.mailbox_messages_before, 0);
    assert_eq!(second.mailbox_messages_after, 0);
}

#[test]
fn reset_during_scope_is_not_reported_as_an_ordinary_delta() {
    let mut runtime = Runtime::new();
    let actor = spawn_handle_actor(&mut runtime);
    runtime.send_message(actor, "handle", &[]);
    runtime.run_scheduler();

    let (_, report) = runtime.explain_analyze(|rt| rt.reset_scheduler_stats());
    assert!(report.scheduler_counter_reset);
    assert_eq!(report.scheduler.tasks_processed, 0);
    assert!(report.render_text().contains("reset detected"));
}

#[test]
fn mailbox_pressure_is_sorted_and_includes_drained_actors() {
    let mut runtime = Runtime::new();
    let one = spawn_handle_actor(&mut runtime);
    let two = spawn_handle_actor(&mut runtime);

    runtime.send_message(one, "handle", &[]);
    runtime.send_message(two, "handle", &[]);
    runtime.send_message(two, "handle", &[]);

    let (_, report) = runtime.explain_analyze(|rt| rt.run_scheduler());
    assert_eq!(report.mailbox_messages_before, 3);
    assert_eq!(report.mailbox_messages_after, 0);
    assert_eq!(report.mailboxes.len(), 2);
    assert_eq!(report.mailboxes[0].actor_id, two);
    assert_eq!(report.mailboxes[0].pending_before, 2);
    assert_eq!(report.mailboxes[0].pending_after, 0);
    assert_eq!(report.mailboxes[1].actor_id, one);
    assert_eq!(report.mailboxes[1].pending_before, 1);
}

#[test]
fn sampling_does_not_run_or_drain_pending_actor_work_by_itself() {
    let mut runtime = Runtime::new();
    let actor = spawn_handle_actor(&mut runtime);
    runtime.send_message(actor, "handle", &[]);

    let (_, report) = runtime.explain_analyze(|_| ());
    assert_eq!(report.scheduler.tasks_processed, 0);
    assert_eq!(report.mailbox_messages_before, 1);
    assert_eq!(report.mailbox_messages_after, 1);
    assert_eq!(runtime.metrics_snapshot().actors_mailboxes.iter().find(|m| m.actor_id == actor).unwrap().depth, 1);
}
