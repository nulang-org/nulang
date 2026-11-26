use nulang::bytecode::Constant;
use nulang::lexer::Lexer;
use nulang::mir_codegen::compile_mir_with_web_field_layout;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::web::handler_call::{
    render_typed_bound_handler, TypedBoundRouteArgument, TypedWebValue,
};

fn compiled(src: &str) -> (nulang::mir_codegen::CompiledWebModule, usize) {
    let ast = Parser::new(Lexer::new(src).lex().unwrap())
        .parse_module()
        .unwrap();
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).unwrap();
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).unwrap();
    let compiled = compile_mir_with_web_field_layout(&mut mir, "typed-form-handler").unwrap();
    let offset = compiled
        .module()
        .debug_functions
        .iter()
        .find(|f| f.name == "endpoint")
        .unwrap()
        .code_offset;
    let function_index = compiled
        .module()
        .function_table
        .iter()
        .position(|p| *p == offset)
        .unwrap();
    (compiled, function_index)
}

fn option_source() -> &'static str {
    r#"
type Option[T] = Some(T) | None
fn ahead() -> Int { let r = { something_else: 7 } in r.something_else }
fn endpoint(note: Option[String]) -> String {
    match note with { | Some(value) => value | None => "missing" }
}
"#
}

fn arg(handler_index: usize, value: TypedWebValue) -> TypedBoundRouteArgument {
    TypedBoundRouteArgument {
        handler_index,
        value,
    }
}

#[test]
fn compiler_owned_field_ids_are_not_guessed_from_record_order() {
    let (compiled, _) = compiled(option_source());
    assert!(compiled.record_field_id("something_else").is_some());
    let ctor = compiled.record_field_id("ctor").unwrap();
    let payload = compiled.record_field_id("payload").unwrap();
    assert_ne!(ctor, payload);
    assert_ne!(compiled.record_field_id("something_else"), Some(ctor));
    assert_ne!(compiled.record_field_id("something_else"), Some(payload));
}

#[test]
fn some_and_none_are_real_compiled_variant_values() {
    let (compiled, idx) = compiled(option_source());
    let some = render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[arg(
            0,
            TypedWebValue::OptionScalar(Some(Constant::String("ok".to_owned()))),
        )],
    )
    .unwrap();
    assert_eq!(some, "ok");
    let none = render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[arg(0, TypedWebValue::OptionScalar(None))],
    )
    .unwrap();
    assert_eq!(none, "missing");
}

#[test]
fn arrays_use_vm_heap_and_preserve_typed_element_order() {
    let (compiled, idx) = compiled("fn endpoint(items: [Int]) -> Int { items[0] * 10 + items[1] }");
    let result = render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[arg(
            0,
            TypedWebValue::ArrayScalars(vec![Constant::Int(7), Constant::Int(4)]),
        )],
    )
    .unwrap();
    assert_eq!(result, "74");
}

#[test]
fn typed_materialization_rejects_missing_field_metadata_instead_of_guessing() {
    let (compiled, idx) = compiled("fn endpoint(x: String) -> String { x }");
    assert!(render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[arg(
            0,
            TypedWebValue::OptionScalar(Some(Constant::String("x".into())))
        )],
    )
    .is_err());
}

#[test]
fn typed_materialization_rejects_ambiguous_or_oversized_argument_bank() {
    let (compiled, idx) = compiled("fn endpoint(x: Int) -> Int { x }");
    assert!(render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[
            arg(0, TypedWebValue::Scalar(Constant::Int(1))),
            arg(0, TypedWebValue::Scalar(Constant::Int(2)))
        ]
    )
    .is_err());
    assert!(render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[arg(1, TypedWebValue::Scalar(Constant::Int(1)))]
    )
    .is_err());
    assert!(render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[arg(
            0,
            TypedWebValue::ArrayScalars(vec![Constant::Int(0); 257])
        )]
    )
    .is_err());
}

#[test]
fn compiled_code_module_schema_remains_unchanged_and_separate_from_layout() {
    let (compiled, _) = compiled(option_source());
    let encoded = serde_json::to_value(&compiled.module()).unwrap();
    assert!(encoded.get("record_field_ids").is_none());
    assert!(encoded.get("web_field_layout").is_none());
    let decoded: nulang::bytecode::CodeModule = serde_json::from_value(encoded).unwrap();
    assert_eq!(&decoded, compiled.module());
}

#[test]
fn typed_array_rejects_heterogeneous_or_out_of_range_scalar_values() {
    let (compiled, idx) = compiled("fn endpoint(items: [Int]) -> Int { items[0] }");
    let mixed = render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[arg(
            0,
            TypedWebValue::ArrayScalars(vec![
                Constant::Int(3),
                Constant::String("unexpected".into()),
            ]),
        )],
    );
    assert!(
        mixed.is_err(),
        "mixed runtime element types must not be materialized"
    );
    let overflow = render_typed_bound_handler(
        &compiled,
        idx,
        1,
        &[arg(
            0,
            TypedWebValue::ArrayScalars(vec![Constant::Int(i64::MAX)]),
        )],
    );
    assert!(
        overflow.is_err(),
        "i64 overflow must not truncate into VM int48"
    );
}
