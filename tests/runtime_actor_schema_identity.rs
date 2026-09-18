use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::Runtime;
use nulang::typechecker::TypeChecker;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lowering");
    nulang::mir_codegen::compile_mir(&mut mir, "schema_identity").expect("bytecode codegen")
}

#[test]
fn module_spawn_preserves_exact_actor_schema_name() {
    let module = compile(
        r#"
        actor First {
            behavior hit() { nil }
        }

        actor Second {
            behavior hit() { nil }
        }
        "#,
    );

    let second = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "Second")
        .expect("Second actor metadata");
    let behavior_idx = *second
        .behavior_indices
        .first()
        .expect("Second has a behavior");

    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, behavior_idx, vec![])
        .as_actor_id()
        .expect("module spawn returns actor ref");

    let actor = runtime.actors.get(&actor_id).expect("spawned actor");
    assert_eq!(
        actor.name, "Second",
        "runtime actor identity must preserve its owning schema rather than only actor_<id>"
    );
}

#[test]
fn manually_spawned_actor_keeps_runtime_instance_name() {
    let mut runtime = Runtime::new();
    let actor_id = runtime.spawn_actor(Box::new(Vec::new));
    let actor = runtime.actors.get(&actor_id).expect("spawned actor");

    assert!(
        actor.name.starts_with("actor_"),
        "manual/native actors must not acquire a fabricated module schema"
    );
}
