#![cfg(feature = "sqlite")]
//! A compiler-identified actor transition crossing an actual libSQL reopen.
//! This proves storage/recovery contracts, not that the bytecode dispatcher
//! already stages all handler effects in one atomic transaction.

use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::durable_effect::{
    DurableEffectId, DurableEffectRecord, DurableEffectRecoveryAction, DurableEffectSpec,
};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, Runtime, DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;

const ACTOR_ID: u64 = 7_401;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("lower MIR");
    nulang::mir_codegen::compile_mir(&mut mir, "typed_durable_restart").expect("compile bytecode")
}

fn counter_module() -> nulang::bytecode::CodeModule {
    compile(
        r#"
        persistent actor Counter {
            state durable count: Int = 0
            behavior add(amount: Int) { self.count = self.count + amount }
        }
        "#,
    )
}

fn register_module(runtime: &mut Runtime, module: nulang::bytecode::CodeModule) {
    let offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.code_offset)
        .collect();
    let compensation_offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.compensate_offset)
        .collect();
    runtime.register_recovery_module(ACTOR_ID, module, offsets, compensation_offsets);
}

fn prepared_effect() -> DurableEffectRecord {
    let spec = DurableEffectSpec::new(
        DurableEffectId::derive(ACTOR_ID, "turn:1", 0, "Audit.append"),
        "Audit.append",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectRecord::prepare(spec, b"count=5")
}

fn first_turn(module: &nulang::bytecode::CodeModule) -> DurableTransition {
    let counter = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "Counter")
        .expect("compiler-owned Counter metadata");
    let behavior_idx = counter.behavior_indices[0];
    assert_eq!(module.behaviors[behavior_idx].param_count, 1);
    let mut state = HashMap::new();
    state.insert("count".into(), PersistedValue::Int(5));

    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence: 1,
        expected_previous_sequence: 0,
        command: Some(JournalEntry {
            sequence: 1,
            behavior_id: u16::try_from(behavior_idx).expect("valid behavior id"),
            payload: vec![PersistedValue::Int(5)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: ACTOR_ID,
            activation_epoch: 1,
            sequence: 1,
            schema_name: Some(counter.name.clone()),
            state,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![DurableEffectPersistenceRecord::from_effect(prepared_effect())],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: ACTOR_ID + 1,
            ordinal: 0,
            behavior_id: 0,
            payload: vec![PersistedValue::Int(5)],
        }],
    }
}

fn test_path() -> (std::path::PathBuf, std::path::PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("valid wall clock")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-typed-durable-restart-{}-{nonce}",
        std::process::id()
    ));
    (directory.clone(), directory.join("actor.sqlite3"))
}

#[test]
fn compiled_actor_state_command_effect_and_outbox_share_atomic_disk_commit() {
    let (directory, db_path) = test_path();
    fs::create_dir_all(&directory).expect("create store directory");
    let module = counter_module();
    let prepared = prepared_effect();
    let id = prepared.spec().id;

    // Crash boundary 1: a committed turn but no externally dispatched call.
    // An ambiguous acknowledgement of an identical transition is idempotent.
    {
        let mut store = LibsqlStore::new(&db_path).expect("create store");
        let candidate = first_turn(&module);
        let first = store.commit_transition(candidate.clone()).expect("commit turn");
        let retry = store.commit_transition(candidate).expect("retry commit");
        assert_eq!(first, retry);
        assert_eq!(store.latest_sequence(ACTOR_ID), 1);
        assert_eq!(store.read_journal(ACTOR_ID).len(), 1);
    }

    // Entire runtime process-local state is discarded. Recover the source
    // schema from compiler metadata and the durable state from disk.
    {
        let mut recovered = Runtime::new();
        recovered.persistence = Box::new(LibsqlStore::new(&db_path).expect("reopen store"));
        register_module(&mut recovered, module.clone());
        assert_eq!(recovered.recover_actor(ACTOR_ID), Some(ACTOR_ID));
        let actor = &recovered.actors[&ACTOR_ID];
        assert_eq!(actor.name, "Counter");
        assert_eq!(actor.get_state_field("count").and_then(|v| v.as_int()), Some(5));
        assert_eq!(recovered.persistence.read_journal(ACTOR_ID).len(), 1);

        let restored = recovered
            .persistence
            .load_durable_effect(ACTOR_ID, id)
            .expect("load effect")
            .expect("Prepared intent persisted");
        assert_eq!(
            restored
                .effect()
                .recovery_action_for_request(b"count=5")
                .expect("stable request"),
            DurableEffectRecoveryAction::RetryWithDeduplication { operation_id: id }
        );
        assert!(
            restored.effect().recovery_action_for_request(b"count=6").is_err(),
            "recovered request digest must be immutable"
        );

        // Crash boundary 2: a provider result was committed as a new atomic
        // receipt before the process dies. Provider invocation is simulated,
        // never performed by this test.
        let completed = prepared.clone().complete(b"accepted".to_vec());
        recovered
            .persistence
            .commit_transition(DurableTransition {
                version: DURABLE_TRANSITION_VERSION,
                actor_id: ACTOR_ID,
                activation_epoch: 1,
                sequence: 2,
                expected_previous_sequence: 1,
                command: None,
                snapshot: None,
                workflow_events: vec![],
                domain_events: vec![],
                durable_effects: vec![DurableEffectPersistenceRecord::from_effect(completed)],
                outbox: vec![],
            })
            .expect("commit receipt");
    }

    {
        let mut restarted = Runtime::new();
        restarted.persistence = Box::new(LibsqlStore::new(&db_path).expect("reopen receipt"));
        register_module(&mut restarted, module);
        assert_eq!(restarted.recover_actor(ACTOR_ID), Some(ACTOR_ID));
        assert_eq!(
            restarted.actors[&ACTOR_ID]
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(5)
        );
        let restored = restarted
            .persistence
            .load_durable_effect(ACTOR_ID, id)
            .unwrap()
            .expect("Completed receipt");
        assert_eq!(
            restored
                .effect()
                .recovery_action_for_request(b"count=5")
                .unwrap(),
            DurableEffectRecoveryAction::ReplayRecordedResult(b"accepted")
        );
        assert_eq!(restarted.persistence.read_journal(ACTOR_ID).len(), 1);
        assert_eq!(restarted.persistence.latest_sequence(ACTOR_ID), 2);
    }

    fs::remove_dir_all(&directory).expect("remove isolated test store");
}

#[test]
fn incompatible_compiler_schema_cannot_adopt_persisted_actor() {
    let (directory, db_path) = test_path();
    fs::create_dir_all(&directory).expect("create store directory");
    {
        let mut store = LibsqlStore::new(&db_path).expect("create store");
        store.commit_transition(first_turn(&counter_module())).unwrap();
    }

    let incompatible = compile(
        r#"
        persistent actor Other {
            state durable count: Int = 0
            behavior add(amount: Int) { self.count = self.count + amount }
        }
        "#,
    );
    let mut recovered = Runtime::new();
    recovered.persistence = Box::new(LibsqlStore::new(&db_path).expect("reopen store"));
    register_module(&mut recovered, incompatible);
    assert_eq!(
        recovered.recover_actor(ACTOR_ID),
        None,
        "the same numeric behavior slot cannot authorize a different actor schema"
    );
    assert!(!recovered.actors.contains_key(&ACTOR_ID));
    assert_eq!(recovered.persistence.latest_sequence(ACTOR_ID), 1);
    drop(recovered);
    fs::remove_dir_all(&directory).expect("remove isolated test store");
}
