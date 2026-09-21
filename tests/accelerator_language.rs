use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::types::{NuResult, Type};
use nulang::vm::{Value, VM};

fn check(source: &str) -> NuResult<Type> {
    let tokens = Lexer::new(source).lex()?;
    let module = Parser::new(tokens).parse_module()?;
    TypeChecker::new().check_module(&module)
}

fn run(source: &str) -> NuResult<Value> {
    let tokens = Lexer::new(source).lex()?;
    let module = Parser::new(tokens).parse_module()?;
    let mut checker = TypeChecker::new();
    checker.check_module(&module)?;
    let hir = hir_lower::lower_module(&module, &checker.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir)?;
    let module = mir_codegen::compile_mir(&mut mir, "accelerator-language")?;
    let mut vm = VM::new();
    vm.load_module(module);
    vm.run()
}

#[test]
fn tensor_and_device_are_builtin_language_types() {
    let result = check(
        r#"
        fn activate(t: Tensor[Float], d: Device) -> Tensor[Float] {
            perform Tensor.relu(t)
        }
        "#,
    );
    assert!(result.is_ok(), "accelerator types should parse/typecheck: {:?}", result.err());

    assert!(check("fn bad(t: Tensor) { nil }").is_err());
    assert!(check("fn bad(d: Device[Int]) { nil }").is_err());
}

#[test]
fn tensor_from_array_requires_floating_values() {
    let result = check(
        r#"
        fn main() -> Tensor[Float] {
            perform Tensor.from_array([1, 2, 3, 4], 2, 2)
        }
        "#,
    );
    assert!(result.is_err(), "integer arrays must not silently become Float tensors");
}

#[test]
fn tensor_matmul_executes_end_to_end() {
    let value = run(
        r#"
        fn main() -> Float {
            let a: Tensor[Float] = perform Tensor.from_array([1.0, 2.0, 3.0, 4.0], 2, 2)
            let b: Tensor[Float] = perform Tensor.from_array([5.0, 6.0, 7.0, 8.0], 2, 2)
            let c: Tensor[Float] = perform Tensor.matmul(a, b)
            let values: [Float] = perform Tensor.to_array(c)
            values[0]
        }
        "#,
    )
    .expect("tensor matmul program should execute");

    assert_eq!(value.as_float(), Some(19.0));
}

#[test]
fn tensor_shape_and_relu_execute_end_to_end() {
    let value = run(
        r#"
        fn main() -> Int {
            let a: Tensor[Float] = perform Tensor.from_array([-1.0, 2.0, -3.0, 4.0], 2, 2)
            let b: Tensor[Float] = perform Tensor.relu(a)
            let shape: [Int] = perform Tensor.shape(b)
            shape[0] * 10 + shape[1]
        }
        "#,
    )
    .expect("tensor shape program should execute");

    assert_eq!(value.as_int(), Some(22));
}

#[test]
fn compute_device_surface_is_typed_and_executable() {
    let value = run(
        r#"
        fn main() -> String {
            let d: Device = perform Compute.default_device()
            perform Compute.device_name(d)
        }
        "#,
    )
    .expect("compute device program should execute");

    assert!(value.is_ptr(), "runtime device handle currently erases to a heap string");
}
