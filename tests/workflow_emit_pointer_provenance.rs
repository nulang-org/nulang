//! Durable workflow outputs must reject pointers outside the emitting actor's heap.
//! These tests exercise provenance and journal atomicity without external services.
use nulang::runtime::{Runtime, WorkflowActivationId, WorkflowEvent};
use nulang::vm::Value;
use std::collections::HashMap;

fn workflow(rt: &mut Runtime, name: &str) -> u64 {
    let id = rt.spawn_workflow_actor(name, Box::new(Vec::new), HashMap::new());
    assert_ne!(id, 0);
    id
}

fn assert_rejected_without_mutation(rt: &mut Runtime, id: u64, args: &[Value]) {
    let before = serde_json::to_value(rt.persistence.read_workflow_events(id)).unwrap();
    let ordinal = rt.actors[&id].workflow_replay_event_ordinal;
    let log_len = rt.actors[&id].event_log.len();

    let error = rt
        .try_emit_event(id, "WorkflowOutput", args)
        .expect_err("a pointer not owned by this actor must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        serde_json::to_value(rt.persistence.read_workflow_events(id)).unwrap(),
        before,
        "failed serialization must not append or mutate a prior durable record"
    );
    assert_eq!(
        rt.actors[&id].workflow_replay_event_ordinal, ordinal,
        "failed serialization must not consume the replay identity"
    );
    assert_eq!(
        rt.actors[&id].event_log.len(), log_len,
        "failed serialization must not publish an in-memory output"
    );
}

#[test]
fn workflow_output_rejects_live_string_from_another_actor() {
    let mut rt = Runtime::new();
    let owner = workflow(&mut rt, "Owner");
    let other = workflow(&mut rt, "Foreign");
    assert_ne!(owner, other);
    let foreign_string = rt.actors.get_mut(&other).unwrap().allocate_string("other actor");
    assert!(!foreign_string.is_nil());

    assert_rejected_without_mutation(&mut rt, owner, &[foreign_string]);
}

#[test]
fn mixed_local_and_foreign_payloads_reject_atomically_then_allow_same_replay_id() {
    let mut rt = Runtime::new();
    let owner = workflow(&mut rt, "Owner");
    let other = workflow(&mut rt, "Foreign");
    let activation = WorkflowActivationId::new(owner, 97);
    rt.actors.get_mut(&owner).unwrap().current_workflow_activation = Some(activation);

    let local_string = rt.actors.get_mut(&owner).unwrap().allocate_string("local");
    let foreign_string = rt.actors.get_mut(&other).unwrap().allocate_string("foreign");
    assert!(!local_string.is_nil());
    assert!(!foreign_string.is_nil());

    assert_rejected_without_mutation(
        &mut rt,
        owner,
        &[local_string, foreign_string],
    );
    rt.try_emit_event(owner, "WorkflowOutput", &[local_string])
        .expect("a rejected mixed payload must not poison the next valid output");

    assert_eq!(rt.actors[&owner].workflow_replay_event_ordinal, 1);
    let outputs = rt
        .persistence
        .read_workflow_events(owner)
        .into_iter()
        .filter(|entry| matches!(entry, WorkflowEvent::Custom { name, .. } if name == "WorkflowOutput"))
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 1, "exactly one durable output must be committed");
}

#[test]
fn workflow_output_rejects_pointer_to_live_memory_outside_actor_heap() {
    let mut rt = Runtime::new();
    let owner = workflow(&mut rt, "Owner");
    // Keep the backing bytes live for the entire test. This is a real address,
    // but it is not a runtime allocation and has no actor-owned String tag.
    let mut external_bytes = vec![b'n', b'o', b't', 0];
    let outside = unsafe { Value::ptr(external_bytes.as_mut_ptr()) };
    assert!(!outside.is_nil());

    assert_rejected_without_mutation(&mut rt, owner, &[outside]);
}
