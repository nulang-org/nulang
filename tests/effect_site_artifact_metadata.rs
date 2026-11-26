use nulang::bytecode::{CodeModule, Constant, OpCode};
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn compile_module(source: &str) -> CodeModule {
    let tokens = Lexer::new(source).lex().expect("source should lex");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("source should parse");
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("source should type-check");
    let hir = hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).expect("source should lower to MIR");
    mir_codegen::compile_mir(&mut mir, "effect-site-artifact").expect("MIR should compile")
}

#[test]
fn async_effect_metadata_points_at_exact_opcode_after_argument_staging() {
    let module = compile_module(
        r#"
        actor Assistant {
            behavior ask(prompt: String) {
                perform Inference.ask(prompt)
            }
        }
        "#,
    );

    assert_eq!(module.effect_sites.len(), 1);
    let site = &module.effect_sites[0];
    assert_eq!(site.effect_operation, "Inference.ask");
    let instruction = module.instructions[site.pc];
    assert_eq!(instruction.opcode, OpCode::PerformAsync);

    let effect_constant = module
        .constants
        .get(instruction.imm16() as usize)
        .expect("PerformAsync must reference the effect-op constant");
    assert_eq!(
        effect_constant,
        &Constant::String("Inference.ask".to_string())
    );
    assert_eq!(module.effect_site_at(site.pc), Some(site));
    assert_eq!(module.effect_site_at(site.pc.saturating_sub(1)), None);
}

#[test]
fn formatting_does_not_change_artifact_effect_site_id() {
    let compact = compile_module(
        "actor Assistant { behavior ask(prompt: String) { perform Inference.ask(prompt) } }",
    );
    let formatted = compile_module(
        "// formatting only\nactor Assistant {\n  behavior ask(prompt: String) {\n    perform Inference.ask(prompt)\n  }\n}\n",
    );

    assert_eq!(compact.effect_sites.len(), 1);
    assert_eq!(formatted.effect_sites.len(), 1);
    assert_eq!(compact.effect_sites[0].id, formatted.effect_sites[0].id);
    assert_eq!(
        compact.effect_sites[0].effect_operation,
        formatted.effect_sites[0].effect_operation
    );
}

#[test]
fn nbc_roundtrip_preserves_effect_site_metadata_without_opcode_change() {
    let module = compile_module(
        r#"
        actor Assistant {
            behavior ask(prompt: String) {
                perform Inference.ask(prompt)
            }
        }
        "#,
    );
    let original_instruction_words: Vec<_> = module
        .instructions
        .iter()
        .map(|instruction| instruction.encode())
        .collect();

    let bytes = module.to_nbc(None).expect("NBC encode");
    let decoded = CodeModule::from_nbc(&bytes).expect("NBC decode").module;

    assert_eq!(decoded.effect_sites, module.effect_sites);
    assert_eq!(
        decoded
            .instructions
            .iter()
            .map(|instruction| instruction.encode())
            .collect::<Vec<_>>(),
        original_instruction_words
    );
    assert_eq!(
        decoded.instructions[decoded.effect_sites[0].pc].opcode,
        OpCode::PerformAsync
    );
}

#[test]
fn two_same_operation_sites_map_to_two_distinct_effect_opcodes() {
    let module = compile_module(
        r#"
        actor Assistant {
            behavior ask(prompt: String) {
                perform Inference.ask(prompt)
                perform Inference.ask(prompt)
            }
        }
        "#,
    );

    assert_eq!(module.effect_sites.len(), 2);
    assert_ne!(module.effect_sites[0].id, module.effect_sites[1].id);
    assert_ne!(module.effect_sites[0].pc, module.effect_sites[1].pc);
    for site in &module.effect_sites {
        assert_eq!(module.instructions[site.pc].opcode, OpCode::PerformAsync);
        assert_eq!(site.effect_operation, "Inference.ask");
    }
}

#[test]
fn compiled_source_effect_drives_recovery_using_one_stable_operation_id() {
    use nulang::durable_effect::DurableEffectSpec;
    use nulang::durable_effect_runtime::{
        DurableEffectCoordinator, DurableEffectDispatchDecision, DurableEffectRuntimeError,
    };
    use nulang::primitives::{DeliverySemantics, EffectBoundary};
    use nulang::runtime::{MemoryStore, PersistenceStore};

    let source =
        "actor Assistant { behavior ask(prompt: String) { perform Inference.ask(prompt) } }";
    let module = compile_module(source);
    let bytes = module.to_nbc(None).expect("NBC encode");
    let recovered = CodeModule::from_nbc(&bytes).expect("NBC decode").module;
    let site = &recovered.effect_sites[0];
    let spec = DurableEffectSpec::from_compiled_async_site(
        &recovered,
        site.pc,
        42,
        "actor:42:turn:8",
        0,
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    )
    .expect("compiler-produced effect site should be accepted");

    let formatted = compile_module(
        r#"// whitespace
actor Assistant {
  behavior ask(prompt: String) {
    perform Inference.ask(prompt)
  }
}
"#,
    );
    let formatted_spec = DurableEffectSpec::from_compiled_async_site(
        &formatted,
        formatted.effect_sites[0].pc,
        42,
        "actor:42:turn:8",
        0,
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    )
    .expect("formatting preserves semantic site identity");
    assert_eq!(spec, formatted_spec);
    assert_eq!(spec.effect_operation, "Inference.ask");
    // The MIR lowering stage owns the canonical module/behavior identity.
    // Do not guess the MIR module name from the bytecode output filename:
    // those names are not required to match.
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut types = TypeChecker::new();
    types.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mir = mir_lower::lower_module(&hir).unwrap();
    let semantic_sites = nulang::semantic_identity::effect_sites_for_mir(&mir);
    assert_eq!(semantic_sites.len(), 1);
    let canonical_site = semantic_sites[0].id;
    assert_eq!(site.id, *canonical_site.as_bytes());
    assert_eq!(
        spec.id,
        nulang::durable_effect::DurableEffectId::derive_from_site(
            42,
            "actor:42:turn:8",
            canonical_site,
            0,
        )
    );

    let request = b"order-42";
    let mut store = MemoryStore::new();
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert_eq!(
            coordinator.begin(spec.clone(), request).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication {
                operation_id: spec.id
            }
        );
    }
    assert_eq!(store.latest_sequence(42), 1);

    // Simulate the provider accepting a mutation immediately before the
    // process dies without recording a terminal receipt.
    let provider_key = spec.id.idempotency_key();
    let mut provider_results = std::collections::HashMap::new();
    let mut provider_calls = 1;
    let first_result = provider_results
        .entry(provider_key.clone())
        .or_insert_with(|| b"provider-receipt".to_vec())
        .clone();
    {
        let mut recovered_coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert_eq!(
            recovered_coordinator
                .begin(formatted_spec, request)
                .unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication {
                operation_id: spec.id
            }
        );
        // The provider receives a second call but deduplicates the mutation.
        assert_eq!(spec.id.idempotency_key(), provider_key);
        provider_calls += 1;
        let second_result = provider_results
            .entry(provider_key)
            .or_insert_with(|| b"duplicate-mutation".to_vec())
            .clone();
        assert_eq!(second_result, first_result);
        recovered_coordinator
            .complete(spec.id, request, second_result)
            .unwrap();
    }
    assert_eq!(provider_results.len(), 1);
    assert_eq!(provider_calls, 2);
    assert_eq!(store.latest_sequence(42), 2);

    let mut restarted = DurableEffectCoordinator::new(&mut store, 42, 1);
    assert_eq!(
        restarted.begin(spec.clone(), request).unwrap(),
        DurableEffectDispatchDecision::ReplayRecordedResult(b"provider-receipt".to_vec())
    );
    assert!(matches!(
        restarted.begin(spec, b"different-request"),
        Err(DurableEffectRuntimeError::RequestMismatch(_))
    ));
    assert_eq!(store.latest_sequence(42), 2);
}

#[test]
fn compiled_async_effect_rejects_missing_site_before_dispatch() {
    use nulang::durable_effect::{CompiledEffectSpecError, DurableEffectSpec};
    use nulang::primitives::{DeliverySemantics, EffectBoundary};

    let mut module = compile_module(
        "actor Assistant { behavior ask(prompt: String) { perform Inference.ask(prompt) } }",
    );
    let pc = module.effect_sites[0].pc;
    module.effect_sites.clear();

    assert!(matches!(
        DurableEffectSpec::from_compiled_async_site(
            &module, pc, 42, "turn:1", 0,
            EffectBoundary::External,
            DeliverySemantics::AtLeastOnce,
        ),
        Err(CompiledEffectSpecError::MissingSite { pc: missing }) if missing == pc
    ));
}

#[test]
fn compiled_async_effect_rejects_opcode_and_constant_drift() {
    use nulang::durable_effect::{CompiledEffectSpecError, DurableEffectSpec};
    use nulang::primitives::{DeliverySemantics, EffectBoundary};

    let module = compile_module(
        "actor Assistant { behavior ask(prompt: String) { perform Inference.ask(prompt) } }",
    );
    let pc = module.effect_sites[0].pc;
    let create = |module: &CodeModule| {
        DurableEffectSpec::from_compiled_async_site(
            module,
            pc,
            42,
            "turn:1",
            0,
            EffectBoundary::External,
            DeliverySemantics::AtLeastOnce,
        )
    };

    let mut wrong_opcode = module.clone();
    wrong_opcode.instructions[pc].opcode = OpCode::Nop;
    assert!(matches!(
        create(&wrong_opcode),
        Err(CompiledEffectSpecError::InvalidOpcode { .. })
    ));

    let mut wrong_operation = module.clone();
    wrong_operation.effect_sites[0].effect_operation = "Other.ask".into();
    assert!(matches!(
        create(&wrong_operation),
        Err(CompiledEffectSpecError::OperationMismatch { .. })
    ));

    let mut wrong_constant = module.clone();
    let constant_idx = wrong_constant.instructions[pc].imm16() as usize;
    wrong_constant.constants[constant_idx] = Constant::String("Other.ask".into());
    assert!(matches!(
        create(&wrong_constant),
        Err(CompiledEffectSpecError::OperationMismatch { .. })
    ));
}

#[test]
fn compiled_async_effect_rejects_missing_turn_identity_and_distinguishes_occurrences() {
    use nulang::durable_effect::{CompiledEffectSpecError, DurableEffectSpec};
    use nulang::primitives::{DeliverySemantics, EffectBoundary};

    let module = compile_module(
        r#"actor Assistant {
            behavior ask(prompt: String) {
                perform Inference.ask(prompt)
                perform Inference.ask(prompt)
            }
        }"#,
    );
    let build = |pc, key, occurrence| {
        DurableEffectSpec::from_compiled_async_site(
            &module,
            pc,
            42,
            key,
            occurrence,
            EffectBoundary::External,
            DeliverySemantics::AtLeastOnce,
        )
    };
    assert!(matches!(
        build(module.effect_sites[0].pc, "  ", 0),
        Err(CompiledEffectSpecError::MissingExecutionKey)
    ));
    let first = build(module.effect_sites[0].pc, "turn:1", 0).unwrap();
    let second = build(module.effect_sites[1].pc, "turn:1", 0).unwrap();
    let repeated = build(module.effect_sites[0].pc, "turn:1", 1).unwrap();
    assert_ne!(first.id, second.id);
    assert_ne!(first.id, repeated.id);
    assert_eq!(
        first.id,
        build(module.effect_sites[0].pc, "turn:1", 0).unwrap().id
    );
}
