use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::types::NuResult;

fn check(source: &str) -> NuResult<nulang::types::Type> {
    let tokens = Lexer::new(source).lex()?;
    let mut parser = Parser::new(tokens);
    let module = parser.parse_module()?;
    TypeChecker::new().check_module(&module)
}

#[test]
fn duplicate_short_behavior_names_use_receiver_protocol_for_arguments() {
    let result = check(
        r#"
        actor First {
            behavior hit(value: Int) { nil }
        }

        actor Second {
            behavior hit(value: String) { nil }
        }

        let target = spawn Second {} in
            send target hit("second")
        "#,
    );

    assert!(
        result.is_ok(),
        "Second.hit(String) must be checked against Second, not the first global '.hit': {:?}",
        result.err()
    );
}

#[test]
fn duplicate_short_behavior_names_reject_other_actor_signature() {
    let result = check(
        r#"
        actor First {
            behavior hit(value: Int) { nil }
        }

        actor Second {
            behavior hit(value: String) { nil }
        }

        let target = spawn Second {} in
            send target hit(42)
        "#,
    );

    assert!(
        result.is_err(),
        "Second.hit requires String even though First.hit accepts Int"
    );
}

#[test]
fn duplicate_short_behavior_names_preserve_each_nominal_protocol() {
    let first = check(
        r#"
        actor First {
            behavior hit(value: Int) { nil }
        }

        actor Second {
            behavior hit(value: String) { nil }
        }

        let target = spawn First {} in
            send target hit(42)
        "#,
    );
    assert!(
        first.is_ok(),
        "First.hit(Int) should typecheck: {:?}",
        first.err()
    );

    let second = check(
        r#"
        actor First {
            behavior hit(value: Int) { nil }
        }

        actor Second {
            behavior hit(value: String) { nil }
        }

        let target = spawn Second {} in
            send target hit("ok")
        "#,
    );
    assert!(
        second.is_ok(),
        "Second.hit(String) should typecheck independently: {:?}",
        second.err()
    );
}