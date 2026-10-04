use std::collections::HashMap;

use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{
    DurableEffectCoordinator, DurableEffectDispatchDecision,
};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    DurableTransition, JournalEntry, Runtime, WorkflowActivationId, WorkflowEvent,
    WorkflowReplayEventId, DURABLE_TRANSITION_VERSION,
};

fn spawn_test_workflow(name: &str) -> (Runtime, u64, u16) {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_workflow_actor(name, Box::new(Vec::new), HashMap::new());
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("run", |_actor, _args| {});
    let behavior_id = rt
        .behavior_id_for(actor_id, "run")
        .expect("registered workflow behavior must have a stable id");
    (rt, actor_id, behavior_id)
}

fn admit_command(rt: &mut Runtime, actor_id: u64, behavior_id: u16) -> WorkflowActivationId {
    let activation_epoch = rt.actors.get(&actor_id).unwrap().activation_epoch;
    let expected_previous_sequence = rt.persistence.latest_sequence(actor_id);
    let sequence = expected_previous_sequence + 1;
    let activation = WorkflowActivationId::new(actor_id, sequence);

    rt.persistence
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch,
            sequence,
            expected_previous_sequence,
            command: Some(JournalEntry {
                sequence,
                behavior_id,
                payload: Vec::new(),
            }),
            snapshot: None,
            workflow_events: Vec::new(),
            domain_events: Vec::new(),
            durable_effects: Vec::new(),
            outbox: Vec::new(),
        })
        .unwrap();

    let actor = rt.actors.get_mut(&actor_id).unwrap();
    actor.sequence = sequence;
    actor.current_workflow_activation = Some(activation);
    activation
}

fn effect_spec(actor_id: u64, activation: WorkflowActivationId) -> (DurableEffectId, DurableEffectSpec) {
    let effect_id = DurableEffectId::derive(
        actor_id,
        &format!("workflow-activation:{}", activation.command_sequence),
        0,
        "Inference.ask",
    );
    let spec = DurableEffectSpec::new(
        effect_id,
        "Inference.ask",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    );
    (effect_id, spec)
}

fn prepare_effect(
    rt: &mut Runtime,
    actor_id: u64,
    activation: WorkflowActivationId,
) -> DurableEffectId {
    let activation_epoch = rt.actors.get(&actor_id).unwrap().activation_epoch;
    let (effect_id, spec) = effect_spec(actor_id, activation);
    let mut coordinator = DurableEffectCoordinator::new(
        rt.persistence.as_mut(),
        actor_id,
        activation_epoch,
    );
    let decision = coordinator.begin(spec, b"prompt").unwrap();
    assert!(matches!(
        decision,
        DurableEffectDispatchDecision::DispatchAtLeastOnce { operation_id }
            if operation_id == effect_id
    ));
    effect_id
}

#[test]
fn recovery_finds_command_below_prepared_effect_tail() {
    let (mut rt, actor_id, behavior_id) = spawn_test_workflow("RecoverPreparedTail");
    let safe_snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    let activation = admit_command(&mut rt, actor_id, behavior_id);

    prepare_effect(&mut rt, actor_id, activation);
    let tail = rt
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .unwrap();
    assert!(tail.sequence > activation.command_sequence);
    assert_eq!(
        rt.persistence.load_snapshot(actor_id).unwrap().sequence,
        safe_snapshot.sequence
    );

    rt.actors.remove(&actor_id);
    assert_eq!(rt.recover_actor(actor_id), Some(actor_id));
    let actor = rt.actors.get(&actor_id).unwrap();
    assert_eq!(actor.current_workflow_activation, Some(activation));
    assert_eq!(actor.sequence, activation.command_sequence);
    assert_eq!(actor.mailbox.len(), 1);
}

#[test]
fn recovery_finds_command_below_completed_effect_tail() {
    let (mut rt, actor_id, behavior_id) = spawn_test_workflow("RecoverCompletedTail");
    let activation = admit_command(&mut rt, actor_id, behavior_id);
    let effect_id = prepare_effect(&mut rt, actor_id, activation);
    let activation_epoch = rt.actors.get(&actor_id).unwrap().activation_epoch;

    {
        let mut coordinator = DurableEffectCoordinator::new(
            rt.persistence.as_mut(),
            actor_id,
            activation_epoch,
        );
        assert_eq!(
            coordinator
                .complete(effect_id, b"prompt", b"recorded-result".to_vec())
                .unwrap(),
            b"recorded-result".to_vec()
        );
    }

    let tail = rt
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .unwrap();
    assert!(tail.sequence >= activation.command_sequence + 2);

    rt.actors.remove(&actor_id);
    assert_eq!(rt.recover_actor(actor_id), Some(actor_id));
    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .current_workflow_activation,
        Some(activation)
    );
}

#[test]
fn recovery_refuses_legacy_record_beyond_atomic_tail() {
    let (mut rt, actor_id, behavior_id) = spawn_test_workflow("RefuseMixedHistory");
    let activation = admit_command(&mut rt, actor_id, behavior_id);
    prepare_effect(&mut rt, actor_id, activation);
    let tail = rt
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .unwrap();

    rt.persistence
        .append_workflow_event(
            actor_id,
            WorkflowEvent::Custom {
                sequence: tail.sequence + 1,
                replay_id: Some(WorkflowReplayEventId::new(activation, 1)),
                name: "legacy-after-atomic".to_string(),
                args: Vec::new(),
            },
        )
        .unwrap();

    assert!(rt.persistence.latest_sequence(actor_id) > tail.sequence);
    rt.actors.remove(&actor_id);
    assert_eq!(rt.recover_actor(actor_id), None);
}

#[test]
fn recovery_refuses_terminal_record_outside_atomic_tail() {
    let (mut rt, actor_id, behavior_id) = spawn_test_workflow("RefuseMixedTerminal");
    let activation = admit_command(&mut rt, actor_id, behavior_id);
    prepare_effect(&mut rt, actor_id, activation);
    let tail = rt
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .unwrap();

    rt.persistence
        .append_workflow_event(
            actor_id,
            WorkflowEvent::StepCompleted {
                sequence: tail.sequence + 1,
                activation: Some(activation),
                step_name: "run".to_string(),
            },
        )
        .unwrap();

    rt.actors.remove(&actor_id);
    assert_eq!(rt.recover_actor(actor_id), None);
}

#[test]
fn recovery_refuses_multiple_commands_between_safe_snapshot_and_tail() {
    let (mut rt, actor_id, behavior_id) = spawn_test_workflow("RefuseMultipleCommands");
    let first = admit_command(&mut rt, actor_id, behavior_id);
    prepare_effect(&mut rt, actor_id, first);
    let second = admit_command(&mut rt, actor_id, behavior_id);
    assert!(second.command_sequence > first.command_sequence);

    rt.actors.remove(&actor_id);
    assert_eq!(
        rt.recover_actor(actor_id),
        None,
        "recovery must fail closed when more than one admitted command exists beyond the safe snapshot"
    );
}
