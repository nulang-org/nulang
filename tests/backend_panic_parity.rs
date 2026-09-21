//! Backend parity tests for MIR runtime panic.
//!
//! Panic is a diverging language operation. Backends must either preserve the
//! runtime error or reject the program at compile time; returning a normal
//! value (especially nil) is never an acceptable fallback.

#[cfg(any(
    feature = "native-codegen",
    feature = "wasm-backend",
    feature = "wasmfx-backend"
))]
use nulang::bytecode::Constant;
#[cfg(any(
    feature = "native-codegen",
    feature = "wasm-backend",
    feature = "wasmfx-backend"
))]
use nulang::mir::{self, RValue, Terminator};
#[cfg(any(
    feature = "native-codegen",
    feature = "wasm-backend",
    feature = "wasmfx-backend"
))]
use nulang::types::Type;

#[cfg(any(
    feature = "native-codegen",
    feature = "wasm-backend",
    feature = "wasmfx-backend"
))]
fn panic_mir() -> mir::Module {
    let mut builder = mir::FunctionBuilder::new("main", None);
    let panic_dst = builder.add_temp(Type::unit());
    builder.assign(
        panic_dst,
        RValue::Panic("backend_panic_test".to_string()),
    );

    // Deliberately leave executable MIR after Panic. A backend that merely
    // records an error but falls through would return 42, violating divergence.
    let after = builder.add_temp(Type::int());
    builder.assign(after, RValue::Const(Constant::Int(42)));
    builder.terminate(Terminator::Return(Some(after)));

    let mut module = mir::Module::new("backend-panic-parity");
    module.functions.push(builder.build());
    module
}

#[cfg(feature = "native-codegen")]
fn panic_behavior_mir() -> mir::Module {
    let mut builder = mir::FunctionBuilder::new("Worker.run", None);
    let panic_dst = builder.add_temp(Type::unit());
    builder.assign(
        panic_dst,
        RValue::Panic("actor_panic_test".to_string()),
    );
    builder.terminate(Terminator::Return(None));

    let mut module = mir::Module::new("backend-actor-panic-parity");
    module.behaviors.push(builder.build());
    module
}

#[cfg(feature = "native-codegen")]
#[test]
fn native_panic_surfaces_runtime_error_and_does_not_fall_through() {
    let module = nulang::aot::AotModule::compile(&panic_mir())
        .expect("native backend should compile MIR Panic");
    let err = module
        .run()
        .expect_err("native Panic must diverge instead of returning the post-panic value");
    let text = err.to_string();

    assert!(
        text.contains("Panic: backend_panic_test"),
        "native panic must preserve its message, got: {text}"
    );
}

#[cfg(feature = "native-codegen")]
#[test]
fn bytecode_and_native_both_treat_panic_as_error() {
    let mut mir = panic_mir();
    let bytecode = nulang::mir_codegen::compile_mir(&mut mir, "backend-panic-parity")
        .expect("bytecode compile");
    let mut vm = nulang::vm::VM::new();
    vm.load_module(bytecode);

    assert!(vm.run().is_err(), "bytecode Panic must be an error");

    let native = nulang::aot::AotModule::compile(&panic_mir())
        .expect("native compile")
        .run();
    assert!(native.is_err(), "native Panic must be an error");
}

#[cfg(feature = "native-codegen")]
#[test]
fn native_actor_behavior_panic_fails_closed_until_actor_fault_propagation_exists() {
    let err = match nulang::aot::AotModule::compile(&panic_behavior_mir()) {
        Ok(_) => panic!(
            "native actor behavior accepted Panic without a scheduler fault propagation channel"
        ),
        Err(err) => err.to_string(),
    };

    assert!(
        err.contains("native behavior adapter cannot propagate actor faults yet"),
        "unexpected native actor panic restriction: {err}"
    );
}

#[cfg(feature = "wasm-backend")]
#[test]
fn plain_wasm_public_backend_rejects_panic_instead_of_returning_nil() {
    use nulang::backends::{DefaultWasmBackend, WasmBackend};

    let mut backend = DefaultWasmBackend;
    let err = backend
        .compile(&panic_mir(), "backend-panic-parity")
        .expect_err("plain WASM must fail closed on unsupported Panic");
    assert!(
        err.to_string().contains("runtime panic is not supported yet"),
        "unexpected WASM restriction error: {err}"
    );
}

#[cfg(feature = "wasm-backend")]
#[test]
fn plain_wasm_low_level_emitter_also_rejects_panic() {
    let mut backend = nulang::mir_wasm::WasmBackend::new();
    let err = backend
        .compile(&panic_mir(), "backend-panic-parity")
        .expect_err("low-level MIR->WASM emitter must not bypass panic restriction");
    assert!(
        err.to_string().contains("runtime panic is not supported yet"),
        "unexpected low-level WASM restriction error: {err}"
    );
}

#[cfg(feature = "wasmfx-backend")]
#[test]
fn wasmfx_rejects_panic_instead_of_lowering_it_to_nil() {
    let mut backend = nulang::wasmfx_backend::WasmFxBackend::new();
    let err = backend
        .compile(&panic_mir(), "backend-panic-parity")
        .expect_err("WasmFX must fail closed on unsupported Panic");
    assert!(
        err.to_string().contains("runtime panic is not supported yet"),
        "unexpected WasmFX restriction error: {err}"
    );
}
