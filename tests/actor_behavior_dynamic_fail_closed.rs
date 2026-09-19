use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

#[test]
fn ambiguous_dynamic_local_behavior_fails_closed() {
    let source = r#"
        actor First {
            behavior hit(value: Int) { nil }
        }

        actor Second {
            behavior hit(value: Int) { nil }
        }

        fn relay(target) {
            send target hit(1)
        }
    "#;

    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let error = nulang::mir_lower::lower_module(&hir)
        .expect_err("ambiguous dynamic local behavior must not select the first suffix match");

    let message = error.to_string();
    assert!(message.contains("behavior 'hit' is ambiguous"), "{message}");
    assert!(message.contains("First.hit"), "{message}");
    assert!(message.contains("Second.hit"), "{message}");
}