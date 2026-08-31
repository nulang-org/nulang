//! Wasmtime-based host runtime for WasmFX modules (`--backend wasmfx-run`).
//!
//! Instantiates modules produced by [`crate::wasmfx_backend::WasmFxBackend`]
//! with stack switching enabled, defines the tag imports the module may
//! reference, and calls `nulang_init`.
//!
//! # Suspension limitation (wasmtime 46)
//!
//! wasmtime 46 implements the WasmFX instruction set in the compiler and
//! validator, but does **not** expose a public host API for receiving a
//! suspended continuation and resuming it later (the stack-switching
//! machinery is `pub(crate)` VM internals). Consequently:
//!
//! - Non-suspending programs run to completion and return their result.
//! - Suspending programs (modules containing `suspend`) trap at the first
//!   suspension; the trap is surfaced as a descriptive [`NuError`] instead
//!   of a silent misbehavior. A future wasmtime with a public continuation
//!   API unlocks the full event loop (the CIR + codegen layers already
//!   produce the correct WasmFX structure).

use crate::types::{NuError, NuResult};
use crate::value_layout;
use wasmtime::*;

/// Create a Wasmtime `Config` with WasmFX stack switching enabled.
pub fn wasmfx_config() -> Config {
    let mut config = Config::new();
    // Guard pages: reserve 4 GiB virtual, 128 MiB guard (same as
    // wasm_runtime::default_wasm_config).
    config.memory_reservation(4 << 30);
    config.memory_guard_size(128 << 20);
    // WASM SIMD proposal.
    config.wasm_simd(true);
    // WasmFX stack switching plus its feature dependencies. Note: wasmtime
    // force-disables compiler inlining when stack switching is enabled.
    config.wasm_stack_switching(true);
    config.wasm_function_references(true);
    // Exception tags are the payload vehicle for `suspend`; requires the
    // wasmtime "gc" feature (enabled by the `wasmfx-backend` cargo feature).
    config.wasm_exceptions(true);
    config
}

// ── Host state ─────────────────────────────────────────────────────────

#[derive(Default)]
struct HostState {
    /// Next allocation offset in WASM linear memory (bump allocator).
    alloc_offset: u32,
    /// Reference to the linear memory, stored for access from host functions.
    memory: Option<Memory>,
}

// ── WasmFX Runtime ─────────────────────────────────────────────────────

/// A compiled and instantiated WasmFX module ready to run.
pub struct WasmFxRuntime {
    _engine: Engine,
    store: Store<HostState>,
    /// The `nulang_init` export function.
    init_func: TypedFunc<(), i64>,
}

impl WasmFxRuntime {
    /// Compile WasmFX bytecode and instantiate with host imports.
    pub fn new(wasm_bytes: &[u8]) -> NuResult<Self> {
        let config = wasmfx_config();
        let engine = Engine::new(&config).map_err(map_wasmtime_err)?;

        let module = Module::new(&engine, wasm_bytes).map_err(map_wasmtime_err)?;

        let mut store = Store::new(&engine, HostState::default());

        let mut linker: Linker<HostState> = Linker::new(&engine);
        linker
            .func_wrap("env", "nulang_alloc", host_alloc)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "nulang_dispatch", host_dispatch)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "log", host_log)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "io_print", host_print)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "io_read", host_read)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "nulang_emit", host_emit)
            .map_err(map_wasmtime_err)?;

        // Define the suspension tag imports. A module that never suspends
        // simply ignores these extra linker entries.
        let tag_type = TagType::new(FuncType::new(&engine, [ValType::I64], []));
        for name in [
            "tag_llm_ask",
            "tag_signal_wait",
            "tag_mailbox_dequeue",
            "tag_perform_async",
            "tag_host_effect",
        ] {
            let tag = Tag::new(&mut store, &tag_type).map_err(map_wasmtime_err)?;
            linker
                .define(&mut store, "env", name, tag)
                .map_err(map_wasmtime_err)?;
        }

        // Provide memory: 1-page (64KB) linear memory.
        let mem_type = MemoryType::new(1, None);
        let memory = Memory::new(&mut store, mem_type).map_err(map_wasmtime_err)?;
        store.data_mut().memory = Some(memory.clone());
        linker
            .define(&mut store, "env", "memory", memory)
            .map_err(map_wasmtime_err)?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(map_wasmtime_err)?;

        // Initialize bump allocator offset to after data segments.
        if let Some(ref exported_mem) = store.data().memory {
            let data_end = exported_mem.data_size(&store);
            store.data_mut().alloc_offset = data_end as u32;
        }

        let init_func = instance
            .get_typed_func::<(), i64>(&mut store, "nulang_init")
            .map_err(map_wasmtime_err)?;

        Ok(WasmFxRuntime {
            _engine: engine,
            store,
            init_func,
        })
    }

    /// Execute the module's `nulang_init` function, returning the tagged result.
    pub fn run(&mut self) -> NuResult<crate::vm::Value> {
        // A root-level `suspend` (no suspender established) unwinds through
        // the call; guard against panics in the fiber machinery as well as
        // the trap Result.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.init_func.call(&mut self.store, ())
        }));
        match result {
            Ok(Ok(raw)) => Ok(crate::vm::Value::from_raw(raw as u64)),
            Ok(Err(trap)) => {
                let msg = format!(
                    "wasm module suspended or trapped: {} \
                     (host-side continuation resume is not exposed by wasmtime 46; \
                     wasmfx-run supports non-suspending programs only)",
                    trap
                );
                Err(NuError::VMError {
                    msg,
                    span: crate::types::Span::default(),
                })
            }
            Err(_) => Err(NuError::VMError {
                msg: "wasm module panicked during execution (likely a root-level \
                      WasmFX suspend; host-side continuation resume is not exposed \
                      by wasmtime 46)"
                    .into(),
                span: crate::types::Span::default(),
            }),
        }
    }
}

// ── Host import functions ──────────────────────────────────────────────

/// `env.io_print(offset: i32, len: i32) -> i64`
fn host_print(mut caller: Caller<'_, HostState>, offset: i32, len: i32) -> Result<i64, Error> {
    let mem = get_memory(&mut caller)?;
    let data = mem.data(&caller);
    let off = offset as usize;
    let end = std::cmp::min(off + len as usize, data.len());
    let text = String::from_utf8_lossy(&data[off..end]);
    print!("{}", text);
    Ok(value_layout::TAG_UNIT as i64)
}

/// `env.io_read() -> i64`
fn host_read(_caller: Caller<'_, HostState>) -> Result<i64, Error> {
    Ok(value_layout::TAG_NIL as i64)
}

/// `env.log(offset: i32, len: i32) -> i64`
fn host_log(mut caller: Caller<'_, HostState>, offset: i32, len: i32) -> Result<i64, Error> {
    let mem = get_memory(&mut caller)?;
    let data = mem.data(&caller);
    let off = offset as usize;
    let end = std::cmp::min(off + len as usize, data.len());
    let text = String::from_utf8_lossy(&data[off..end]);
    eprintln!("[wasm] {}", text);
    Ok(value_layout::TAG_UNIT as i64)
}

/// `env.nulang_alloc(size: i32) -> i32` — simple bump allocator.
fn host_alloc(mut caller: Caller<'_, HostState>, size: i32) -> Result<i32, Error> {
    let size = (size as u32 + 7) & !7u32; // align to 8
    let offset = caller.data().alloc_offset;
    let required = offset
        .checked_add(size)
        .ok_or_else(|| Error::msg("alloc overflow"))?;
    let mem = get_memory(&mut caller)?;
    let current_size = mem.data_size(&caller) as u32;
    if required > current_size {
        let pages_needed = ((required - current_size) + 65535) / 65536;
        mem.grow(&mut caller, pages_needed as u64)
            .map_err(|e| Error::msg(format!("memory grow: {}", e)))?;
    }
    caller.data_mut().alloc_offset = required;
    Ok(offset as i32)
}

/// `env.nulang_dispatch(a: i32, b: i32, c: i32, d: i32) -> i64` — stub.
fn host_dispatch(_caller: Caller<'_, HostState>, _a: i32, _b: i32, _c: i32, _d: i32) -> i64 {
    0
}

/// `env.nulang_emit(frame_ptr: i32, arg_count: i32) -> i64` — stub.
///
/// Fire-and-forget effect dispatch (Actor.send, event emission, ...).
/// Effects are not wired to the actor runtime in this backend.
fn host_emit(
    _caller: Caller<'_, HostState>,
    _frame_ptr: i32,
    _arg_count: i32,
) -> Result<i64, Error> {
    Ok(value_layout::TAG_UNIT as i64)
}

/// Helper: retrieve linear memory from the HostState.
fn get_memory(caller: &mut Caller<'_, HostState>) -> Result<Memory, Error> {
    caller
        .data()
        .memory
        .clone()
        .ok_or_else(|| Error::msg("env.memory not initialized"))
}

fn map_wasmtime_err(e: wasmtime::Error) -> NuError {
    NuError::VMError {
        msg: format!("wasmtime: {}", e),
        span: crate::types::Span::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: compile a Nulang expression to wasmfx bytes via the full pipeline.
    fn compile_expr(source: &str) -> Vec<u8> {
        let tokens = crate::lexer::Lexer::new(source).lex().unwrap();
        let ast = crate::parser::Parser::new(tokens).parse_module().unwrap();
        let mut tc = crate::typechecker::TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir = crate::mir_lower::lower_module(&hir).unwrap();
        let mut backend = crate::wasmfx_backend::WasmFxBackend::new();
        backend.compile(&mir, "test").unwrap()
    }

    #[test]
    fn test_run_literal_int() {
        let wasm = compile_expr("42");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        // 42 tagged as int
        assert_eq!(result.as_raw(), crate::value_layout::tag_int(42));
    }

    #[test]
    fn test_run_addition() {
        let wasm = compile_expr("1 + 2");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_int(3));
    }

    #[test]
    fn test_run_multiplication() {
        let wasm = compile_expr("4 * 5");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_int(20));
    }

    #[test]
    fn test_run_subtraction() {
        let wasm = compile_expr("10 - 3");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_int(7));
    }

    #[test]
    fn test_run_bool_true() {
        let wasm = compile_expr("true");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_bool(true));
    }

    #[test]
    fn test_run_bool_false() {
        let wasm = compile_expr("false");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_bool(false));
    }

    #[test]
    fn test_run_comparison_eq() {
        let wasm = compile_expr("1 == 1");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_bool(true));
    }

    #[test]
    fn test_run_comparison_neq() {
        let wasm = compile_expr("1 != 2");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_bool(true));
    }

    #[test]
    fn test_run_comparison_lt() {
        let wasm = compile_expr("1 < 2");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_bool(true));
    }

    #[test]
    fn test_run_comparison_gt() {
        let wasm = compile_expr("2 > 1");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_bool(true));
    }

    #[test]
    fn test_run_let_binding() {
        let wasm = compile_expr("let x = 10; x + 5");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::tag_int(15));
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_run_float() {
        let wasm = compile_expr("3.14");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        // Floats in Nulang are raw IEEE-754 bits (no tag masking).
        let bits = f64::from_bits(result.as_raw());
        assert!((bits - 3.14).abs() < 0.001, "expected ~3.14, got {}", bits);
    }

    #[test]
    fn test_run_nil() {
        let wasm = compile_expr("nil");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::TAG_NIL as u64);
    }

    #[test]
    fn test_run_unit() {
        let wasm = compile_expr("()");
        let mut runtime = WasmFxRuntime::new(&wasm).expect("instantiate");
        let result = runtime.run().expect("run");
        assert_eq!(result.as_raw(), crate::value_layout::TAG_UNIT as u64);
    }
}
