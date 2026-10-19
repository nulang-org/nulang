use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::types::NuResult;

fn check(source: &str) -> NuResult<nulang::types::Type> {
    let tokens = Lexer::new(source).lex()?;
    let module = Parser::new(tokens).parse_module()?;
    TypeChecker::new().check_module(&module)
}

#[test]
fn actor_ref_accepts_concrete_actor_with_extra_behaviors() {
    let result = check(
        r#"
        actor Counter {
            state n: Int = 0
            behavior get() -> Int { self.n }
            behavior add(value: Int) -> Unit { nil }
            behavior reset() -> Unit { nil }
        }

        fn read(target: ActorRef[{ get: () -> Int }]) -> Int {
            ask target get()
        }

        let counter = spawn Counter {} in read(counter)
        "#,
    );

    assert!(
        result.is_ok(),
        "a concrete actor may expose more behaviors than the required ActorRef protocol: {:?}",
        result.err()
    );
}

#[test]
fn actor_ref_rejects_concrete_actor_missing_required_behavior() {
    let result = check(
        r#"
        actor Counter {
            behavior get() -> Int { 0 }
        }

        fn reset(target: ActorRef[{ reset: () -> Unit }]) -> Unit {
            send target reset()
        }

        let counter = spawn Counter {} in reset(counter)
        "#,
    );

    assert!(result.is_err(), "Counter does not advertise reset()");
}

#[test]
fn actor_ref_preserves_behavior_argument_arity() {
    let two_args = check(
        r#"
        actor Sink {
            behavior push(x: Int, label: String) -> Unit { nil }
        }

        fn push(target: ActorRef[{ push: (Int, String) -> Unit }]) -> Unit {
            send target push(1, "one")
        }

        let sink = spawn Sink {} in push(sink)
        "#,
    );
    assert!(
        two_args.is_ok(),
        "a two-argument behavior should satisfy a two-argument structural protocol: {:?}",
        two_args.err()
    );

    let tuple_arg = check(
        r#"
        actor Sink {
            behavior push(pair: (Int, String)) -> Unit { nil }
        }

        fn push(target: ActorRef[{ push: (Int, String) -> Unit }]) -> Unit {
            send target push(1, "one")
        }

        let sink = spawn Sink {} in push(sink)
        "#,
    );
    assert!(
        tuple_arg.is_err(),
        "one tuple-valued argument must not be confused with two message arguments"
    );
}

#[test]
fn actor_ref_tuple_protocol_parameter_is_an_argument_pack() {
    let ok = check(
        r#"
        fn push(target: ActorRef[{ push: (Int, String) -> Unit }]) -> Unit {
            send target push(1, "one")
        }
        "#,
    );
    assert!(
        ok.is_ok(),
        "(Int, String) in an ActorRef behavior signature denotes two message arguments: {:?}",
        ok.err()
    );

    let wrong_shape = check(
        r#"
        fn push(target: ActorRef[{ push: (Int, String) -> Unit }]) -> Unit {
            send target push((1, "one"))
        }
        "#,
    );
    assert!(
        wrong_shape.is_err(),
        "a tuple payload is one argument and must not satisfy a two-argument protocol"
    );
}

#[test]
fn actor_ref_rejects_concrete_actor_with_incompatible_signature() {
    let result = check(
        r#"
        actor Counter {
            behavior add(value: String) -> Unit { nil }
        }

        fn add_one(target: ActorRef[{ add: Int -> Unit }]) -> Unit {
            send target add(1)
        }

        let counter = spawn Counter {} in add_one(counter)
        "#,
    );

    assert!(
        result.is_err(),
        "a concrete behavior signature must match the required ActorRef signature"
    );
}

#[test]
fn calls_through_actor_ref_reject_unknown_behavior() {
    let result = check(
        r#"
        fn misuse(target: ActorRef[{ get: () -> Int }]) -> Unit {
            send target reset()
        }
        "#,
    );

    assert!(
        result.is_err(),
        "abstract ActorRef calls must be limited to declared protocol members"
    );
}

#[test]
fn calls_through_actor_ref_check_argument_types() {
    let result = check(
        r#"
        fn misuse(target: ActorRef[{ add: Int -> Unit }]) -> Unit {
            send target add("wrong")
        }
        "#,
    );

    assert!(
        result.is_err(),
        "abstract ActorRef calls must enforce behavior parameter types"
    );
}

#[test]
fn ask_through_actor_ref_propagates_return_type() {
    let ok = check(
        r#"
        fn read(target: ActorRef[{ get: () -> Int }]) -> Int {
            ask target get()
        }
        "#,
    );
    assert!(
        ok.is_ok(),
        "ask should return the protocol result type: {:?}",
        ok.err()
    );

    let bad = check(
        r#"
        fn read(target: ActorRef[{ get: () -> Int }]) -> String {
            ask target get()
        }
        "#,
    );
    assert!(
        bad.is_err(),
        "ActorRef ask result must participate in ordinary return-type checking"
    );
}

#[test]
fn unannotated_behavior_cannot_satisfy_typed_actor_ref() {
    let result = check(
        r#"
        actor Counter {
            behavior add(value) { nil }
        }

        fn add_one(target: ActorRef[{ add: Int -> Unit }]) -> Unit {
            send target add(1)
        }

        let counter = spawn Counter {} in add_one(counter)
        "#,
    );

    assert!(
        result.is_err(),
        "structural protocols must fail closed when the concrete behavior signature is not declared"
    );
}

#[test]
fn actor_ref_and_concrete_protocols_are_erased_before_hir() {
    use nulang::hir::Decl as HirDecl;
    use nulang::types::Type;

    let source = r#"
        actor Counter {
            behavior get() -> Int { 0 }
        }

        fn read(target: ActorRef[{ get: () -> Int }]) -> Int {
            ask target get()
        }
    "#;

    let tokens = Lexer::new(source).lex().expect("lex");
    let module = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&module).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&module, &checker.inferred_decl_types);

    let read = hir
        .decls
        .iter()
        .find_map(|decl| match decl {
            HirDecl::Function(function) if function.name == "read" => Some(function),
            _ => None,
        })
        .expect("read function");

    let (_, target_ty) = read.params.first().expect("target param");
    assert!(
        target_ty.actor_ref_protocol().is_none(),
        "ActorRef protocol metadata must not survive HIR lowering"
    );
    match target_ty {
        Type::Actor { state, behavior } => {
            assert_eq!(state.as_ref(), &Type::unit());
            assert_eq!(behavior.as_ref(), &Type::unit());
        }
        other => panic!("ActorRef must lower to the stable runtime actor shape, got {other}"),
    }

    let counter = hir
        .decls
        .iter()
        .find_map(|decl| match decl {
            HirDecl::Actor(actor) if actor.name == "Counter" => Some(actor),
            _ => None,
        })
        .expect("Counter actor");

    for behavior in &counter.behaviors {
        for (_, ty) in &behavior.params {
            assert!(
                ty.actor_ref_protocol().is_none(),
                "nested actor protocol metadata must not survive in behavior params"
            );
        }
    }
}

#[test]
fn actor_ref_can_be_attenuated_to_a_narrower_abstract_protocol() {
    let result = check(
        r#"
        fn narrow(
            target: ActorRef[{ get: () -> Int, add: Int -> Unit }]
        ) -> ActorRef[{ get: () -> Int }] {
            target
        }

        fn read(target: ActorRef[{ get: () -> Int }]) -> Int {
            ask target get()
        }

        fn run(target: ActorRef[{ get: () -> Int, add: Int -> Unit }]) -> Int {
            read(narrow(target))
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "an ActorRef exposing a protocol superset should flow to a narrower requirement: {:?}",
        result.err()
    );
}

#[test]
fn actor_ref_cannot_be_widened_to_require_missing_behaviors() {
    let result = check(
        r#"
        fn widen(
            target: ActorRef[{ get: () -> Int }]
        ) -> ActorRef[{ get: () -> Int, add: Int -> Unit }] {
            target
        }
        "#,
    );

    assert!(
        result.is_err(),
        "ActorRef attenuation must be directional; a narrower reference cannot be widened"
    );
}

#[test]
fn actor_ref_function_arguments_support_safe_attenuation() {
    let ok = check(
        r#"
        fn read(target: ActorRef[{ get: () -> Int }]) -> Int {
            ask target get()
        }

        fn run(target: ActorRef[{ get: () -> Int, add: Int -> Unit }]) -> Int {
            read(target)
        }
        "#,
    );
    assert!(
        ok.is_ok(),
        "function arguments should accept ActorRef protocol supersets: {:?}",
        ok.err()
    );

    let bad = check(
        r#"
        fn mutate(target: ActorRef[{ get: () -> Int, add: Int -> Unit }]) -> Unit {
            send target add(1)
        }

        fn run(target: ActorRef[{ get: () -> Int }]) -> Unit {
            mutate(target)
        }
        "#,
    );
    assert!(
        bad.is_err(),
        "function arguments must reject ActorRef values missing required behaviors"
    );
}

#[test]
fn actor_ref_annotation_supports_attenuation_but_not_widening() {
    let narrow = check(
        r#"
        fn narrow(target: ActorRef[{ get: () -> Int, add: Int -> Unit }]) -> ActorRef[{ get: () -> Int }] {
            target : ActorRef[{ get: () -> Int }]
        }
        "#,
    );
    assert!(
        narrow.is_ok(),
        "explicit annotation should permit attenuation"
    );

    let widen = check(
        r#"
        fn widen(target: ActorRef[{ get: () -> Int }]) -> ActorRef[{ get: () -> Int, add: Int -> Unit }] {
            target : ActorRef[{ get: () -> Int, add: Int -> Unit }]
        }
        "#,
    );
    assert!(widen.is_err(), "explicit annotation must reject widening");
}

#[test]
fn actor_ref_attenuation_still_checks_behavior_signatures() {
    let result = check(
        r#"
        fn use_int(target: ActorRef[{ add: Int -> Unit }]) -> Unit {
            send target add(1)
        }

        fn run(target: ActorRef[{ add: String -> Unit, get: () -> Int }]) -> Unit {
            use_int(target)
        }
        "#,
    );

    assert!(
        result.is_err(),
        "matching behavior names are insufficient when their signatures differ"
    );
}

#[test]
fn actor_ref_requires_a_record_protocol() {
    let tokens = Lexer::new("fn bad(target: ActorRef[Int]) { nil }")
        .lex()
        .expect("lex");
    let result = Parser::new(tokens).parse_module();
    assert!(
        result.is_err(),
        "ActorRef[Int] is not a structural protocol"
    );
}
