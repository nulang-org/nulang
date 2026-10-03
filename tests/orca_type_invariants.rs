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
fn mutually_recursive_arrays_are_rejected() {
    // Two distinct arrays can form a cycle only if their element types satisfy
    // mutually recursive constraints: A = [B] and B = [A]. The occurs check
    // must reject that indirect infinite type just as it rejects T = [T].
    let result = typecheck(
        r#"
        fn main() {
            let left = []
            let right = []
            left[0] = right
            right[0] = left
        }
        "#,
    );

    assert!(
        result.is_err(),
        "mutually recursive structural array types must be rejected"
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

#[test]
fn empty_array_binding_is_monomorphic_after_first_store() {
    // Arrays are mutable storage. Once one store constrains the element type,
    // later uses of the same binding must observe that same element type.
    let result = typecheck(
        r#"
        fn main() {
            let xs = []
            xs[0] = 1
            xs[0] = "string"
        }
        "#,
    );

    assert!(
        result.is_err(),
        "one mutable array binding must not be instantiated at incompatible element types"
    );
}

#[test]
fn independent_empty_array_bindings_do_not_share_type_variables() {
    let result = typecheck(
        r#"
        fn main() {
            let ints = []
            let strings = []
            ints[0] = 1
            strings[0] = "string"
            0
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "independent mutable arrays should infer independently: {:?}",
        result.err()
    );
}

#[test]
fn array_factory_remains_polymorphic_per_call() {
    // The function value itself is generalized. Each invocation allocates a
    // distinct mutable array, so each returned array may acquire its own
    // monomorphic element type without sharing storage typing across calls.
    let result = typecheck(
        r#"
        fn main() {
            let make = fn() { [] }
            let ints = make()
            let strings = make()
            ints[0] = 1
            strings[0] = "string"
            0
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "array-producing functions should remain polymorphic per call: {:?}",
        result.err()
    );
}

#[test]
fn aliasing_empty_array_does_not_regain_polymorphism() {
    let result = typecheck(
        r#"
        fn main() {
            let xs = []
            let alias = xs
            xs[0] = 1
            alias[0] = "string"
        }
        "#,
    );

    assert!(
        result.is_err(),
        "aliases of one mutable array must retain one shared element type"
    );
}
