use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{ActorSnapshot, GrainId, PersistenceStore, Runtime};
use nulang::typechecker::TypeChecker;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lowering");
    nulang::mir_codegen::compile_mir(&mut mir, "runtime_snapshot_schema_identity")
        .expect("bytecode codegen")
}

fn register_for_recovery(rt: &mut Runtime, actor_id: u64, module: nulang::bytecode::CodeModule) {
    let offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.code_offset as usize)
        .collect();
    let compensation_offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.compensate_offset.map(|offset| offset as usize))
        .collect();
    rt.register_recovery_module(actor_id, module, offsets, compensation_offsets);
}

#[test]
fn legacy_snapshot_json_without_schema_name_remains_readable() {
    let snapshot: ActorSnapshot = serde_json::from_str(
        r#"{
            "actor_id": 7,
            "sequence": 3,
            "state": {},
            "waiting_signal": null,
            "crdt_snapshot": null,
            "crdt_field_map": null,
            "authority_tokens": []
        }"#,
    )
    .expect("legacy snapshot json");
    assert_eq!(snapshot.actor_id, 7);
    assert_eq!(snapshot.schema_name, None);
}

#[test]
fn recovery_restores_only_the_persisted_actor_schema() {
    let module = compile(
        r#"
        persistent actor First {
            state durable first_only: Int = 11
            behavior hit() -> Int { self.first_only }
        }
        persistent actor Second {
            state durable second_only: Int = 22
            behavior hit() -> Int { self.second_only }
        }
        "#,
    );
    let actor_id = 7001;
    let mut rt = Runtime::new();
    register_for_recovery(&mut rt, actor_id, module);
    rt.persistence
        .save_snapshot(ActorSnapshot {
            actor_id,
            sequence: 0,
            state: Default::default(),
            waiting_signal: None,
            crdt_snapshot: None,
            crdt_field_map: None,
            schema_name: Some("Second".to_string()),
            authority_tokens: Default::default(),
        })
        .expect("snapshot");

    assert_eq!(rt.recover_actor(actor_id), Some(actor_id));
    let actor = &rt.actors[&actor_id];
    assert_eq!(actor.name, "Second");
    assert!(actor.get_state_field("first_only").is_none());
    assert_eq!(
        actor
            .get_state_field("second_only")
            .and_then(|value| value.as_int()),
        Some(22)
    );
}

#[test]
fn recovery_rejects_unknown_or_ambiguous_schema_identity() {
    let module = compile(
        r#"
        persistent actor First { behavior hit() { nil } }
        persistent actor Second { behavior hit() { nil } }
        "#,
    );

    let mut unknown = Runtime::new();
    register_for_recovery(&mut unknown, 7002, module.clone());
    unknown
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id: 7002,
            schema_name: Some("Missing".to_string()),
            ..Default::default()
        })
        .expect("snapshot");
    assert_eq!(unknown.recover_actor(7002), None);

    let mut legacy = Runtime::new();
    register_for_recovery(&mut legacy, 7003, module);
    legacy
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id: 7003,
            ..Default::default()
        })
        .expect("legacy snapshot");
    assert_eq!(legacy.recover_actor(7003), None);
}

#[test]
fn grain_hydration_rejects_a_different_valid_module_schema() {
    let module = compile(
        r#"
        virtual entity Counter(key: String) {
            state durable n: Int = 0
            behavior hit() { self.n = self.n + 1 }
        }
        actor Other { behavior hit() { nil } }
        "#,
    );
    let grain_id = GrainId::new("Counter", "customer-42");
    let mut rt = Runtime::new();
    rt.register_module_grains(&module);
    let stable_id = rt
        .resolve_or_hydrate_grain(grain_id.clone())
        .expect("fresh grain");
    rt.actors.remove(&stable_id);
    rt.grain_residents.remove(&grain_id);
    rt.actor_grain_id.remove(&stable_id);

    rt.persistence
        .save_snapshot(ActorSnapshot {
            actor_id: stable_id,
            schema_name: Some("Other".to_string()),
            ..Default::default()
        })
        .expect("mismatched grain snapshot");

    let error = rt
        .resolve_or_hydrate_grain(grain_id)
        .expect_err("Counter must reject an Other snapshot");
    assert!(error.to_string().contains("does not match expected schema"));
}


#[test]
fn migration_uses_persisted_workflow_schema_for_role_and_compensation_layout() {
    let module = compile(
        r#"
        workflow FirstFlow {
            step first { nil } compensate { nil }
        }

        workflow SecondFlow {
            step alpha { nil }
            step beta { nil } compensate { nil }
        }
        "#,
    );
    let second = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "SecondFlow")
        .expect("SecondFlow metadata");
    let expected_compensation: Vec<Option<usize>> = second
        .behavior_indices
        .iter()
        .map(|&i| module.behaviors[i].compensate_offset.map(|offset| offset as usize))
        .collect();
    let first_len = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "FirstFlow")
        .expect("FirstFlow metadata")
        .behavior_indices
        .len();
    assert_ne!(
        first_len,
        expected_compensation.len(),
        "fixture must distinguish first-workflow fallback from selected schema"
    );

    let actor_id = 7004;
    let snapshot = ActorSnapshot {
        actor_id,
        schema_name: Some("SecondFlow".to_string()),
        ..Default::default()
    };
    let snapshot_json = serde_json::to_vec(&snapshot).expect("snapshot json");
    let nbc = module.to_nbc(None).expect("nbc");

    let mut rt = Runtime::new();
    assert!(rt.receive_migrated_actor(actor_id, nbc, snapshot_json));

    let actor = &rt.actors[&actor_id];
    assert_eq!(actor.name, "SecondFlow");
    assert!(actor.is_workflow);
    assert_eq!(actor.compensation_offsets, expected_compensation);
}
