#![cfg(feature = "native-codegen")]

use nulang::aot::AotModule;
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::types::NuError;
use nulang::vm::{Value, VM};

const HOT_SLICE_READ: &str = r#"
fn main() -> Int {
    let source = [10, 20, 30, 40]
    let slice = perform Array.slice(source, 1, 3)
    let state = [0, 0]

    while state[0] < 1200 {
        state[1] = state[1] + slice[0] + slice[1]
        state[0] = state[0] + 1
    }

    state[1]
}
"#;

const SLICE_COW_STORE: &str = r#"
fn main() -> Int {
    let source = [10, 20, 30, 40]
    let slice = perform Array.slice(source, 1, 3)
    slice[0] = 99
    slice[0] * 100 + slice[1] + source[1]
}
"#;

fn lower(source: &str) -> Result<mir::Module, NuError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.lex()?;
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module()?;

    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast)?;

    let hir = hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    mir_lower::lower_module(&hir)
}

fn compile_bytecode(source: &str) -> Result<nulang::bytecode::CodeModule, NuError> {
    let mut mir = lower(source)?;
    mir_codegen::compile_mir(&mut mir, "array-view-backend-parity")
}

fn run_interpreter(source: &str) -> Result<Value, NuError> {
    let module = compile_bytecode(source)?;
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    vm.run()
}

fn run_jit(source: &str) -> Result<Value, NuError> {
    let module = compile_bytecode(source)?;
    let mut vm = VM::new();
    vm.load_module(module);
    vm.run()
}

fn run_aot(source: &str) -> Result<Value, NuError> {
    let mir = lower(source)?;
    let module = AotModule::compile(&mir)?;
    module.run().map(|raw| unsafe { Value::from_raw(raw) })
}

fn assert_all_backends_int(source: &str, expected: i64) {
    let interpreter = run_interpreter(source).expect("interpreter execution");
    let jit = run_jit(source).expect("tiered JIT execution");
    let aot = run_aot(source).expect("AOT execution");

    assert_eq!(interpreter.as_int(), Some(expected));
    assert_eq!(jit.as_int(), Some(expected));
    assert_eq!(aot.as_int(), Some(expected));
    assert_eq!(interpreter.as_raw(), jit.as_raw());
    assert_eq!(interpreter.as_raw(), aot.as_raw());
}

#[test]
fn hot_array_view_loads_match_interpreter_jit_and_aot() {
    // 1,200 loop iterations cross the normal JIT hot threshold (1,000),
    // ensuring the view load path is exercised after tier-up.
    assert_all_backends_int(HOT_SLICE_READ, 60_000);
}

#[test]
fn array_view_cow_store_matches_interpreter_jit_and_aot() {
    // slice becomes [99, 30] while source must remain [10, 20, 30, 40].
    assert_all_backends_int(SLICE_COW_STORE, 9_950);
}
