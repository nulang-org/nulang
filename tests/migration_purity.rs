use nulang::effect_checker::EffectChecker;
use nulang::lexer::Lexer;
use nulang::parser::Parser;

fn check(source: &str) -> Result<(), nulang::types::NuError> {
    let tokens = Lexer::new(source).lex()?;
    let ast = Parser::new(tokens).parse_module()?;
    let mut checker = EffectChecker::new();
    checker.check_module(&ast.decls)
}

#[test]
fn migration_cannot_launder_perform_through_handler() {
    let source = r#"
        entity Counter {
            version: 2
            state count: Int = 0
            migration from 1 to 2 {
                state => {
                    handle { perform IO.print("hidden") } { | IO.print(msg) => unit }
                }
            }
        }
    "#;
    let err = check(source).unwrap_err().to_string();
    assert!(err.contains("perform IO.print"), "unexpected error: {err}");
}

#[test]
fn migration_rejects_direct_extern_call() {
    let source = r#"
        extern "libm.so.6" { fn sqrt(x: Float) -> Float }
        entity Counter {
            version: 2
            state value: Float = 0.0
            migration from 1 to 2 {
                state => { self.value = sqrt(self.value) }
            }
        }
    "#;
    let err = check(source).unwrap_err().to_string();
    assert!(
        err.contains("external/FFI call 'sqrt'"),
        "unexpected error: {err}"
    );
}

#[test]
fn migration_rejects_transitive_effectful_helper() {
    let source = r#"
        fn level2() { perform IO.print("x") }
        fn level1() { level2() }
        entity Counter {
            version: 2
            state count: Int = 0
            migration from 1 to 2 {
                state => { level1() }
            }
        }
    "#;
    let err = check(source).unwrap_err().to_string();
    assert!(err.contains("perform IO.print"), "unexpected error: {err}");
}

#[test]
fn migration_allows_pure_helper_and_replay_emit() {
    let source = r#"
        fn normalize(x: Int) { x + 1 }
        entity Counter {
            version: 2
            state count: Int = 0
            events
                | Bumped(by: Int)
            migration from 1 to 2 {
                state => { self.count = normalize(self.count) }
                events {
                    | Bumped(by) => emit Bumped(normalize(by))
                }
            }
        }
    "#;
    check(source).expect("pure migration should pass strict migration checker");
}
