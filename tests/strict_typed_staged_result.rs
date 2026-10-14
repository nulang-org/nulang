//! Strict, host-supplied atomic completion and quarantine recovery coverage.
//! The host never executes user bytecode or external effects within this API.

use std::collections::HashMap;
use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    DurableOutboxMessage, DurableTransition, JournalEntry, PersistedValue, PersistenceStore,
    Runtime, DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn spawned_counter() -> (Runtime, u64, u16) {
    let tokens = Lexer::new(
        r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
        "#,
    )
    .lex()
    .expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR");
    let module = nulang::mir_codegen::compile_mir(&mut mir, "typed_result").expect("bytecode");
    let behavior = module.actor_metadata[0].behavior_indices[0] as u16;
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, behavior as usize, vec![])
        .as_actor_id()
        .expect("spawn persistent counter");
    (runtime, actor_id, behavior)
}

fn admit(runtime: &mut Runtime, actor_id: u64, behavior: u16, amount: i64) -> u64 {
    runtime
        .enable_strict_typed_actor_admission(actor_id)
        .expect("persist strict gate before accepting user commands");
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    let epoch = runtime.actors[&actor_id].activation_epoch;
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(amount)]);
    let commit = runtime
        .admit_next_typed_actor_command(
            actor_id,
            DurableTransition {
                version: DURABLE_TRANSITION_VERSION,
                actor_id,
                activation_epoch: epoch,
                sequence: predecessor + 1,
                expected_previous_sequence: predecessor,
                command: Some(JournalEntry {
                    sequence: predecessor + 1,
                    behavior_id: behavior,
                    payload: vec![PersistedValue::Int(amount)],
                }),
                snapshot: None,
                workflow_events: vec![],
                domain_events: vec![],
                durable_effects: vec![],
                outbox: vec![],
            },
        )
        .unwrap()
        .expect("admit command");
    assert_eq!(commit.0.payload[0].as_int(), Some(amount));
    commit.1.sequence
}

fn prepared_effect(actor_id: u64) -> DurableEffectPersistenceRecord {
    let spec = DurableEffectSpec::new(
        DurableEffectId::derive(actor_id, "cmd:1", 0, "Audit::Write"),
        "Audit::Write",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectPersistenceRecord::from_effect(DurableEffectRecord::prepare(spec, b"amount=5"))
}

#[test]
fn result_state_and_outbox_commit_before_live_state_or_next_command() {
    let (mut runtime, actor_id, behavior) = spawned_counter();
    let admitted = admit(&mut runtime, actor_id, behavior, 5);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(6)]);

    let updates = HashMap::from([("total".to_string(), PersistedValue::Int(5))]);
    let result = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            admitted,
            updates,
            vec![],
            vec![DurableOutboxMessage {
                destination_actor_id: actor_id + 1,
                ordinal: 0,
                behavior_id: 0,
                payload: vec![PersistedValue::Int(5)],
            }],
        )
        .expect("commit staged result");
    assert_eq!(result.sequence, admitted + 1);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted + 1);
    assert_eq!(runtime.persistence.read_journal(actor_id).len(), 1);
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(5)
    );
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        None,
        "durably completed command releases its specific completion fence"
    );
    assert_eq!(
        runtime.actors[&actor_id].mailbox.len(),
        1,
        "strict admission retains the next message for explicit durable admission"
    );
    runtime.step_actor(actor_id);
    assert_eq!(
        runtime.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(5),
        "ordinary scheduler cannot bypass strict admission after completion"
    );
}

#[test]
fn invalid_typed_result_never_commits_or_releases_admission_fence() {
    let (mut runtime, actor_id, behavior) = spawned_counter();
    let admitted = admit(&mut runtime, actor_id, behavior, 5);

    let error = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            admitted,
            HashMap::from([(
                "total".to_string(),
                PersistedValue::String("wrong".into()),
            )]),
            vec![],
            vec![],
        )
        .expect_err("reject invalid typed result before persistence");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(admitted)
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0)
    );
}

#[test]
fn prepared_external_effect_is_denied_without_live_capability_grant() {
    let (mut runtime, actor_id, behavior) = spawned_counter();
    let admitted = admit(&mut runtime, actor_id, behavior, 5);
    let record = prepared_effect(actor_id);
    let operation_id = record.effect().spec().id;
    let error = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            admitted,
            HashMap::from([("total".to_string(), PersistedValue::Int(5))]),
            vec![record],
            vec![],
        )
        .expect_err("external effect requires live authority");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted);
    assert!(
        runtime
            .persistence
            .load_durable_effect(actor_id, operation_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn result_commit_requires_an_actual_admitted_command() {
    let (mut runtime, actor_id, _behavior) = spawned_counter();
    let error = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            1,
            HashMap::new(),
            vec![],
            vec![],
        )
        .expect_err("no admitted execution");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), 0);
}

#[test]
fn exact_actor_grant_allows_prepared_intent_but_never_dispatches_a_provider() {
    use nulang::authority::{AuthorityGrant, AuthorityManifest};

    let (mut runtime, actor_id, behavior) = spawned_counter();
    let grant = "Audit::Write"
        .parse::<AuthorityGrant>()
        .expect("canonical typed tool grant");
    runtime
        .actors
        .get_mut(&actor_id)
        .unwrap()
        .install_authority_manifest(&AuthorityManifest::from_grants([grant]));
    let admitted = admit(&mut runtime, actor_id, behavior, 5);
    let intent = prepared_effect(actor_id);
    let effect_id = intent.effect().spec().id;

    runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            admitted,
            HashMap::from([("total".to_string(), PersistedValue::Int(5))]),
            vec![intent],
            vec![],
        )
        .expect("preauthorized Prepared intent may be committed");

    let recovered = runtime
        .persistence
        .load_durable_effect(actor_id, effect_id)
        .unwrap()
        .expect("committed intent");
    assert!(matches!(
        recovered.effect(),
        DurableEffectRecord::Prepared { .. }
    ));
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted + 1);
}

#[test]
fn repeated_external_effect_id_is_rejected_before_result_commit() {
    use nulang::authority::{AuthorityGrant, AuthorityManifest};

    let (mut runtime, actor_id, behavior) = spawned_counter();
    let grant = "Audit::Write".parse::<AuthorityGrant>().unwrap();
    runtime
        .actors
        .get_mut(&actor_id)
        .unwrap()
        .install_authority_manifest(&AuthorityManifest::from_grants([grant]));
    let admitted = admit(&mut runtime, actor_id, behavior, 5);
    let intent = prepared_effect(actor_id);
    let error = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            admitted,
            HashMap::from([("total".to_string(), PersistedValue::Int(5))]),
            vec![intent.clone(), intent],
            vec![],
        )
        .expect_err("one logical external effect cannot have two records");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(admitted)
    );
}

#[test]
fn non_strict_admission_cannot_use_strict_staged_completion() {
    let (mut runtime, actor_id, behavior) = spawned_counter();
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    let epoch = runtime.actors[&actor_id].activation_epoch;
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let (_, admitted) = runtime.admit_next_typed_actor_command(
        actor_id,
        DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: epoch,
            sequence: predecessor + 1,
            expected_previous_sequence: predecessor,
            command: Some(JournalEntry {
                sequence: predecessor + 1,
                behavior_id: behavior,
                payload: vec![PersistedValue::Int(5)],
            }),
            snapshot: None,
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        },
    ).unwrap().unwrap();
    let error = runtime.commit_admitted_typed_actor_result(
        actor_id,
        admitted.sequence,
        HashMap::from([("total".to_owned(), PersistedValue::Int(5))]),
        vec![],
        vec![],
    ).expect_err("strict staged completion requires persisted strict admission policy");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(admitted.sequence)
    );
}

#[test]
fn a_quarantined_command_accepts_trusted_staged_result_without_bytecode_replay() {
    let (mut runtime, actor_id, behavior) = spawned_counter();
    runtime.checkpoint_actor(actor_id);
    let safe_snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();
    let module = runtime.actors[&actor_id].bytecode_module.as_ref().unwrap().as_ref().clone();
    let offsets = module.behaviors.iter().map(|b| b.code_offset as usize).collect();
    let compensation = module.behaviors.iter().map(|b| b.compensate_offset).collect();
    runtime.register_recovery_module(actor_id, module, offsets, compensation);
    let admitted = admit(&mut runtime, actor_id, behavior, 5);
    runtime.actors.remove(&actor_id);
    assert_eq!(runtime.recover_actor(actor_id), Some(actor_id));
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(admitted)
    );
    let after_recovery = runtime.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(after_recovery.sequence, safe_snapshot.sequence);
    assert_eq!(after_recovery.activation_epoch, safe_snapshot.activation_epoch);
    assert_eq!(after_recovery.state, safe_snapshot.state);

    let result = runtime.commit_admitted_typed_actor_result(
        actor_id,
        admitted,
        HashMap::from([("total".to_owned(), PersistedValue::Int(5))]),
        vec![],
        vec![],
    ).expect("strict host can complete quarantined command atomically");
    assert_eq!(result.sequence, admitted + 1);
    assert_eq!(
        runtime.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(5)
    );
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        None
    );
    assert_eq!(runtime.persistence.read_journal(actor_id).len(), 1);
}

#[test]
fn previously_persisted_external_effect_id_cannot_be_staged_again() {
    use nulang::authority::{AuthorityGrant, AuthorityManifest};

    let (mut runtime, actor_id, behavior) = spawned_counter();
    let grant = "Audit::Write".parse::<AuthorityGrant>().unwrap();
    runtime.actors.get_mut(&actor_id).unwrap()
        .install_authority_manifest(&AuthorityManifest::from_grants([grant]));
    let prior = prepared_effect(actor_id);
    let spec_id = prior.effect().spec().id;
    let epoch = runtime.actors[&actor_id].activation_epoch;

    runtime.persistence.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: epoch,
        sequence: 1,
        expected_previous_sequence: 0,
        command: None,
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![prior.clone()],
        outbox: vec![],
    }).expect("persist previous logical effect");

    let admitted = admit(&mut runtime, actor_id, behavior, 5);
    let error = runtime.commit_admitted_typed_actor_result(
        actor_id,
        admitted,
        HashMap::from([("total".to_string(), PersistedValue::Int(5))]),
        vec![prior],
        vec![],
    ).expect_err("historical effect ID must not be minted twice");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(runtime.persistence.load_durable_effect(actor_id, spec_id)
        .unwrap().is_some());
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(admitted)
    );
}
