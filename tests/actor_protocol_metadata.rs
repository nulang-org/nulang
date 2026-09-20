use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("mir lower");
    nulang::mir_codegen::compile_mir(&mut mir, "protocol-metadata").expect("codegen")
}

fn actor_protocol(source: &str, actor: &str) -> [u8; 32] {
    compile(source)
        .actor_metadata
        .into_iter()
        .find(|meta| meta.name == actor)
        .and_then(|meta| meta.protocol_id)
        .expect("compiled actor protocol id")
}

#[test]
fn actor_meta_protocol_id_is_independent_of_behavior_body() {
    let first = actor_protocol(
        r#"
        actor Account {
            behavior balance(id: Int) -> Int { id }
        }
        "#,
        "Account",
    );
    let second = actor_protocol(
        r#"
        actor Account {
            behavior balance(id: Int) -> Int { id + 1 }
        }
        "#,
        "Account",
    );

    assert_eq!(
        first, second,
        "implementation-only changes must not change the structural actor protocol"
    );
}

#[test]
fn actor_meta_protocol_id_changes_with_behavior_signature() {
    let first = actor_protocol(
        r#"
        actor Account {
            behavior balance(id: Int) -> Int { id }
        }
        "#,
        "Account",
    );
    let changed_param = actor_protocol(
        r#"
        actor Account {
            behavior balance(id: String) -> Int { 0 }
        }
        "#,
        "Account",
    );
    let changed_return = actor_protocol(
        r#"
        actor Account {
            behavior balance(id: Int) -> String { "ok" }
        }
        "#,
        "Account",
    );

    assert_ne!(first, changed_param);
    assert_ne!(first, changed_return);
}

#[test]
fn actor_meta_protocol_id_is_independent_of_behavior_declaration_order() {
    let first = actor_protocol(
        r#"
        actor Account {
            behavior balance() -> Int { 0 }
            behavior deposit(amount: Int) -> Unit { nil }
        }
        "#,
        "Account",
    );
    let reordered = actor_protocol(
        r#"
        actor Account {
            behavior deposit(amount: Int) -> Unit { nil }
            behavior balance() -> Int { 0 }
        }
        "#,
        "Account",
    );

    assert_eq!(first, reordered);
}
