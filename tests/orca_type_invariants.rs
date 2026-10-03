use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn typecheck(source: &str) -> nulang::types::NuResult<nulang::types::Type> {
    let tokens = Lexer::new(source).lex()?;
    let module = Parser::new(tokens).parse_module()?;
    TypeChecker::new().check_module(&module)
}

#[test]
fn empty_array_cannot_be_made_directly_self_referential() {
    // A direct local reference cycle would require the element type T to
    // satisfy T = [T]. HM unification must reject that infinite type rather
    // than allowing user code to manufacture an intra-actor RC cycle.
    let tokens = Lexer::new(
        r#"
        fn main() {
            let xs = []
            xs[0] = xs
        }
        "#,
    )
    .lex()
    .expect("self-cycle regression must lex");
    let module = Parser::new(tokens)
        .parse_module()
        .expect("self-cycle regression must parse");

    let err = TypeChecker::new()
        .check_module(&module)
        .expect_err("T = [T] must be rejected by the occurs check");

    let diagnostic = err.to_string().to_lowercase();
    assert!(
        diagnostic.contains("infinite") || diagnostic.contains("occurs"),
        "expected an infinite-type/occurs-check diagnostic, got: {err}"
    );
}

#[test]
fn concrete_array_cannot_store_itself_as_an_element() {
    // Even without relying on an unconstrained empty-array element variable,
    // an ordinary typed array cannot be stored into an element slot of a
    // different type. This pins the user-language side of ORCA's assumption
    // that direct structural self-cycles are not constructible.
    let result = typecheck(
        r#"
        fn main() {
            let xs = [0]
            xs[0] = xs
        }
        "#,
    );

    assert!(
        result.is_err(),
        "a [Int] value must not be storable into its Int element slot"
    );
}

#[test]
fn normal_nested_arrays_remain_well_typed() {
    // Guard against over-tightening the type system while protecting the cycle
    // invariant: acyclic nested containers must remain valid.
    let result = typecheck(
        r#"
        fn main() {
            let inner = [1, 2]
            let outer = [inner]
            outer
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "acyclic nested arrays should remain valid: {:?}",
        result.err()
    );
}
