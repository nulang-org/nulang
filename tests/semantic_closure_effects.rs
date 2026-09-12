//! Semantic-closure regression tests for algebraic-effect backend parity.
//!
//! These tests deliberately distinguish two contracts:
//! 1. language features accepted by multiple backends must agree; and
//! 2. a restricted backend must reject unsupported continuation forms
//!    deterministically rather than silently changing semantics.

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

const IMPLICIT_RESUME: &str = r#"
effect Tick { next: Int -> Int }

fn main() -> Int {
    handle {
        let x = perform Tick.next(40)
        x + 2
    } {
        | Tick.next(x) resume => x + 1
    }
}
"#;

const ABORTIVE_HANDLER: &str = r#"
effect Tick { next: Int -> Int }

fn main() -> Int {
    handle {
        let x = perform Tick.next(40)
        x + 100
    } {
        | Tick.next(x) => x + 1
    }
}
"#;

const NESTED_INNERMOST_HANDLER: &str = r#"
effect Shared { get: Int -> Int }

fn main() -> Int {
    handle {
        handle {
            perform Shared.get(0)
        } {
            | Shared.get(x) resume => 1
        }
    } {
        | Shared.get(x) resume => 2
    }
}
"#;

const SEQUENTIAL_RESUMES: &str = r#"
effect Math { double: Int -> Int }

fn main() -> Int {
    handle {
        let a = perform Math.double(3)
        let b = perform Math.double(10)
        let c = perform Math.double(a + b)
        c
    } {
        | Math.double(n) resume => n * 2
    }
}
"#;

const EXPLICIT_RESUME_EXPR: &str = r#"
effect Tick { next: Int -> Int }

fn main() -> Int {
    handle {
        let x = perform Tick.next(40)
        x + 2
    } {
        | Tick.next(x) resume => resume(x + 1)
    }
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

fn run_bytecode(source: &str) -> Result<Value, NuError> {
    let mut mir = lower(source)?;
    let module = mir_codegen::compile_mir(&mut mir, "semantic-closure-effects")?;
    let mut vm = VM::new();
    vm.load_module(module);
    vm.run()
}

fn run_native(source: &str) -> Result<Value, NuError> {
    let mir = lower(source)?;
    let module = AotModule::compile(&mir)?;
    module.run().map(Value::from_raw)
}

fn assert_bytecode_native_int(source: &str, expected: i64) {
    let bytecode = run_bytecode(source).expect("bytecode should execute semantic-closure case");
    let native = run_native(source).expect("native should execute supported semantic-closure case");

    assert_eq!(bytecode.as_int(), Some(expected));
    assert_eq!(native.as_int(), Some(expected));
    assert_eq!(
        bytecode.as_raw(),
        native.as_raw(),
        "bytecode and native must agree on the exact tagged result"
    );
}

#[test]
fn resumable_handler_matches_bytecode_and_native() {
    assert_bytecode_native_int(IMPLICIT_RESUME, 43);
}

#[test]
fn abortive_handler_matches_bytecode_and_native() {
    // A non-resuming arm aborts the captured continuation. The expression
    // after the perform must therefore not execute; the handler result wins.
    assert_bytecode_native_int(ABORTIVE_HANDLER, 41);
}

#[test]
fn nested_same_effect_uses_innermost_handler_on_both_backends() {
    // Handler lookup is dynamically nested: the inner Shared.get arm must win
    // and resume its own continuation without leaking to the outer handler.
    assert_bytecode_native_int(NESTED_INNERMOST_HANDLER, 1);
}

#[test]
fn sequential_performs_capture_fresh_continuations_on_both_backends() {
    // This formerly stressed the native multi-perform resuming-handler path.
    // Each perform must capture a fresh continuation: 3*2=6, 10*2=20,
    // then (6+20)*2=52.
    assert_bytecode_native_int(SEQUENTIAL_RESUMES, 52);
}

#[test]
fn explicit_resume_expression_is_a_deterministic_native_restriction() {
    let bytecode = run_bytecode(EXPLICIT_RESUME_EXPR)
        .expect("bytecode should execute explicit continuation resume");
    assert_eq!(bytecode.as_int(), Some(43));

    let mir = lower(EXPLICIT_RESUME_EXPR).expect("explicit resume should lower to MIR");
    let err = match AotModule::compile(&mir) {
        Ok(_) => panic!(
            "native accepted explicit resume(expr); if support was implemented, replace this restricted-profile assertion with a differential result check"
        ),
        Err(err) => err.to_string(),
    };

    assert!(
        err.contains("effect-continuation resume requires the bytecode backend"),
        "native must reject explicit resume(expr) with the documented restriction, got: {err}"
    );
}

#[cfg(feature = "wasm-backend")]
#[test]
fn plain_wasm_rejects_resumable_handlers_instead_of_returning_nil() {
    use nulang::backends::{DefaultWasmBackend, WasmBackend};

    let mir = lower(IMPLICIT_RESUME).expect("resumable handler should lower to MIR");
    let mut backend = DefaultWasmBackend;
    let err = backend
        .compile(&mir, "semantic-closure-effects")
        .expect_err("plain WASM must reject unsupported handler/resume semantics");
    let message = err.to_string();

    assert!(
        message.contains("WASM backend restricted profile")
            && message.contains("continuation resume are not supported"),
        "plain WASM must fail loudly instead of compiling Resume to nil, got: {message}"
    );
}

#[cfg(feature = "wasmfx-backend")]
#[test]
fn wasmfx_rejects_user_handlers_instead_of_returning_nil() {
    let mir = lower(IMPLICIT_RESUME).expect("resumable handler should lower to MIR");
    let mut backend = nulang::wasmfx_backend::WasmFxBackend::new();
    let err = backend
        .compile(&mir, "semantic-closure-effects")
        .expect_err("WasmFX must reject unsupported user handler/resume semantics");
    let message = err.to_string();

    assert!(
        message.contains("WasmFX backend restricted profile")
            && message.contains("continuation resume are not supported yet"),
        "WasmFX must fail loudly instead of compiling user-handler Resume to nil, got: {message}"
    );
}
