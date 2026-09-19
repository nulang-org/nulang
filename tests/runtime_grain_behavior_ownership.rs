use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{GrainId, Runtime};
use nulang::typechecker::TypeChecker;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lowering");
    nulang::mir_codegen::compile_mir(&mut mir, "runtime_grain_behavior_ownership")
        .expect("bytecode codegen")
}

#[test]
fn hydrated_grain_name_resolves_through_virtual_actor_schema() {
    let module = compile(
        r#"
        virtual entity Counter(key: String) {
            behavior hit() -> Int { 7 }
        }

        actor Other {
            behavior hit() -> Int { 9 }
        }
        "#,
    );
    let counter_hit = *module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "Counter")
        .and_then(|meta| meta.behavior_indices.first())
        .expect("Counter.hit index");

    let mut runtime = Runtime::new();
    runtime.register_module_grains(&module);
    let actor_id = runtime
        .resolve_or_hydrate_grain(GrainId::new("Counter", "counter-42"))
        .expect("hydrate Counter grain");

    assert_eq!(runtime.actors[&actor_id].name, "Counter@counter-42");
    assert_eq!(
        runtime.behavior_id_for(actor_id, "hit"),
        Some(counter_hit as u16),
        "runtime grain instance names must resolve through their Counter schema"
    );
    assert_eq!(
        runtime.behavior_id_for(actor_id, "Other.hit"),
        None,
        "a grain must not expose another actor schema's qualified behavior"
    );
}