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
