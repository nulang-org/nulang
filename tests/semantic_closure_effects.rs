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

#[test]
fn resumable_handler_matches_bytecode_and_native() {
    let bytecode = run_bytecode(IMPLICIT_RESUME).expect("bytecode should execute resumable handler");
    let native =
        run_native(IMPLICIT_RESUME).expect("native should execute supported resumable handler");

    assert_eq!(bytecode.as_int(), Some(43));
    assert_eq!(native.as_int(), Some(43));
    assert_eq!(bytecode.as_raw(), native.as_raw());
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
