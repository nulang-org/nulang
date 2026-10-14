//! Regression gates for identities staged in one atomic durable turn.
//! No actor scheduler or provider is involved: invalid envelopes must be
//! rejected before any backend can commit state or publish external effects.

use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JournalEntry, MemoryStore, PersistenceStore,
    WorkflowActivationId, WorkflowEvent, WorkflowReplayEventId, DURABLE_TRANSITION_VERSION,
};

const ACTOR_ID: u64 = 42;

fn transition() -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence: 1,
        expected_previous_sequence: 0,
        command: Some(JournalEntry {
            sequence: 1,
            behavior_id: 0,
            payload: vec![],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: ACTOR_ID,
            sequence: 1,
            activation_epoch: 1,
            schema_name: Some("Counter".into()),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

fn effect(ordinal: u32) -> DurableEffectPersistenceRecord {
    let spec = DurableEffectSpec::new(
        DurableEffectId::derive(ACTOR_ID, "counter:turn-1", ordinal, "Audit.write"),
        "Audit.write",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectPersistenceRecord::from_effect(DurableEffectRecord::prepare(spec, b"request"))
}

fn replay(ordinal: u32) -> WorkflowReplayEventId {
    WorkflowReplayEventId::new(WorkflowActivationId::new(ACTOR_ID, 1), ordinal)
}

#[test]
fn duplicate_effect_identity_cannot_commit_or_mutate_state() {
    let mut candidate = transition();
    candidate.durable_effects = vec![effect(0), effect(0)];
    let mut store = MemoryStore::new();

    let error = store.commit_transition(candidate).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("duplicate durable effect"), "{error}");
    assert_eq!(store.latest_sequence(ACTOR_ID), 0);
    assert!(store.load_snapshot(ACTOR_ID).is_none());
    assert!(store.committed_transitions(ACTOR_ID).is_empty());
}

#[test]
fn distinct_effect_receipts_can_commit_together() {
    let mut candidate = transition();
    candidate.durable_effects = vec![effect(0), effect(1)];
    let mut store = MemoryStore::new();

    let commit = store.commit_transition(candidate).expect("valid multi-effect turn");
    assert_eq!(commit.sequence, 1);
    assert_eq!(store.committed_transitions(ACTOR_ID)[0].durable_effects.len(), 2);
    for ordinal in 0..2 {
        let id = effect(ordinal).effect().spec().id;
        assert!(store.load_durable_effect(ACTOR_ID, id).unwrap().is_some());
    }
}

#[test]
fn forged_compensation_link_is_rejected_before_commit() {
    let mut candidate = transition();
    let original_effect_id = effect(0).effect().spec().id;
    let unrelated_effect = DurableEffectRecord::prepare(
        DurableEffectSpec::new(
            DurableEffectId::derive(ACTOR_ID, "another-turn", 9, "Payment.refund"),
            "Payment.refund",
            EffectBoundary::External,
            DeliverySemantics::EffectivelyOnceWithDeduplication,
        ),
        b"refund",
    );
    candidate.durable_effects = vec![DurableEffectPersistenceRecord::Compensation {
        original_effect_id,
        compensation_ordinal: 0,
        effect: unrelated_effect,
    }];
    let mut store = MemoryStore::new();

    let error = store.commit_transition(candidate).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("compensation identity mismatch"), "{error}");
    assert!(store.load_snapshot(ACTOR_ID).is_none());
    assert_eq!(store.latest_sequence(ACTOR_ID), 0);
}

#[test]
fn duplicate_workflow_replay_identity_cannot_commit() {
    let mut candidate = transition();
    candidate.workflow_events = vec![
        WorkflowEvent::TimerSet {
            sequence: 1,
            replay_id: Some(replay(0)),
            name: "deadline".into(),
            duration_ms: 100,
        },
        WorkflowEvent::Custom {
            sequence: 1,
            replay_id: Some(replay(0)),
            name: "Progress".into(),
            args: vec![],
        },
    ];
    let error = candidate.digest().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("duplicate workflow replay"), "{error}");
}

#[test]
fn workflow_replay_identity_cannot_claim_another_actor() {
    let mut candidate = transition();
    candidate.workflow_events.push(WorkflowEvent::TimerSet {
        sequence: 1,
        replay_id: Some(WorkflowReplayEventId::new(
            WorkflowActivationId::new(ACTOR_ID + 1, 1),
            0,
        )),
        name: "wake".into(),
        duration_ms: 100,
    });
    let error = candidate.digest().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("replay activation actor"), "{error}");
}

#[test]
fn workflow_replay_identity_cannot_claim_future_command() {
    let mut candidate = transition();
    candidate.command = None;
    candidate.workflow_events.push(WorkflowEvent::Custom {
        sequence: 1,
        replay_id: Some(WorkflowReplayEventId::new(
            WorkflowActivationId::new(ACTOR_ID, 2),
            0,
        )),
        name: "Future".into(),
        args: vec![],
    });
    let error = candidate.digest().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("future command"), "{error}");
}

#[test]
fn workflow_replay_identity_must_match_admitted_command_when_present() {
    let mut candidate = transition();
    candidate.workflow_events.push(WorkflowEvent::Custom {
        sequence: 1,
        replay_id: Some(WorkflowReplayEventId::new(
            WorkflowActivationId::new(ACTOR_ID, 0),
            0,
        )),
        name: "Wrong".into(),
        args: vec![],
    });
    let error = candidate.digest().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("accepted command"), "{error}");
}

#[test]
fn distinct_workflow_replay_identities_commit_and_remain_replay_stable() {
    let mut candidate = transition();
    candidate.workflow_events = vec![
        WorkflowEvent::TimerSet {
            sequence: 1,
            replay_id: Some(replay(0)),
            name: "deadline".into(),
            duration_ms: 100,
        },
        WorkflowEvent::Custom {
            sequence: 1,
            replay_id: Some(replay(1)),
            name: "Progress".into(),
            args: vec![],
        },
    ];
    let expected_digest = candidate.digest().expect("valid transition");
    let mut store = MemoryStore::new();
    let first = store.commit_transition(candidate.clone()).unwrap();
    let repeated = store.commit_transition(candidate).unwrap();
    assert_eq!(first.digest, expected_digest);
    assert_eq!(first, repeated);
    assert_eq!(store.committed_transitions(ACTOR_ID).len(), 1);
}
