//! Wasmtime-based WASM runtime for Nulang Cloud.
//!
//! Loads `.wasm` modules produced by `mir_wasm::WasmBackend` and executes
//! them with an optimized Wasmtime configuration:
//!
//! - **Memory guard pages**: `memory_reservation(4 GiB)` +
//!   `memory_guard_size(128 MiB)`. Cranelift emits plain `mov` without bounds
//!   checks; the MMU catches OOB as SIGSEGV → Wasmtime trap.
//! - **Cranelift speed**: `cranelift_opt_level(Speed)` enables cross-function
//!   inlining and other optimizations.
//! - **SIMD**: `wasm_simd(true)` enables the WASM SIMD proposal (v128 ops).
//!
//! # Host imports
//!
//! The WASM backend emits modules that import:
//! - `env.memory` — linear memory
//! - `env.nulang_alloc(i32) -> i32` — bump allocator in WASM memory
//! - `env.nulang_dispatch(i32,i32,i32,i32)` — effect dispatch (stub)
//! - `env.log(i32,i32) -> i64` — log to stderr
//! - `env.io_print(i32,i32) -> i64` — print to stdout
//! - `env.io_read() -> i64` — read stdin (stub: returns nil)

use crate::types::Span;
use crate::types::{NuError, NuResult};
use crate::value_layout;
use wasmtime::*;

// ── Default configuration ────────────────────────────────────────────

/// Create a Wasmtime `Config` with Nulang Cloud optimizations.
///
/// Enables:
/// - 4 GiB virtual memory reservation + 128 MiB guard region
/// - Cranelift speed optimizations (includes inlining)
/// - WASM SIMD proposal
pub fn default_wasm_config() -> Config {
    let mut config = Config::new();
    // Guard pages: reserve 4 GiB virtual, 128 MiB guard.
    config.memory_reservation(4 << 30);
    config.memory_guard_size(128 << 20);
    // Cranelift speed optimizations (enables cross-function inlining).
    config.cranelift_opt_level(OptLevel::Speed);
    // WASM SIMD proposal.
    config.wasm_simd(true);
    config
}

// ── Host state ───────────────────────────────────────────────────────

struct HostState {
    /// Next allocation offset in WASM linear memory (bump allocator).
    alloc_offset: u32,
    /// Reference to the linear memory, stored for access from host functions.
    memory: Option<Memory>,
    /// Testable input source for `IO.read`. When non-empty, `host_read` reads
    /// lines from here instead of stdin.
    input: std::sync::Arc<parking_lot::Mutex<Vec<u8>>>,
    /// Injectable effect-dispatch result. When `Some`, `host_dispatch` writes
    /// these bytes to the ring buffer at [`crate::mir_wasm::RING_BUFFER_BASE`]
    /// and returns their length — mirroring the pool's `host_dispatch`
    /// contract so the compiler's dispatch read-back is testable without a
    /// real effect runtime. `None` (the default) = no result (length 0).
    dispatch_result: std::sync::Arc<parking_lot::Mutex<Option<Vec<u8>>>>,
    /// Last (tag, payload) pair passed to `nulang_dispatch`, recorded for
    /// tests to verify the compiler's marshaling. Cleared by
    /// [`WasmRuntime::take_last_dispatch`].
    last_dispatch: std::sync::Arc<parking_lot::Mutex<Option<(Vec<u8>, Vec<u8>)>>>,
}

impl Default for HostState {
    fn default() -> Self {
        HostState {
            alloc_offset: 0,
            memory: None,
            input: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            dispatch_result: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            last_dispatch: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        }
    }
}

// ── WASM Runtime ─────────────────────────────────────────────────────

/// A compiled and instantiated WASM module ready to run.
pub struct WasmRuntime {
    _engine: Engine,
    store: Store<HostState>,
    /// The `nulang_init` export function.
    init_func: TypedFunc<(), i64>,
}

impl WasmRuntime {
    /// Compile WASM bytecode and instantiate with host imports.
    pub fn new(wasm_bytes: &[u8], config: Option<Config>) -> NuResult<Self> {
        let config = config.unwrap_or_else(default_wasm_config);
        let engine = Engine::new(&config).map_err(map_wasmtime_err)?;

        let res = Module::new(&engine, wasm_bytes);
        if let Err(_) = &res {
            std::fs::write("/tmp/failed_module.wasm", wasm_bytes).unwrap();
        }
        let module = res.map_err(map_wasmtime_err)?;

        let mut store = Store::new(&engine, HostState::default());

        // Build a Linker and define all host imports.
        let mut linker: Linker<HostState> = Linker::new(&engine);

        linker
            .func_wrap("env", "nulang_alloc", host_alloc)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "nulang_dispatch", host_dispatch)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "nulang_dispatch_args", host_dispatch_args)
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
            .func_wrap("env", "str_concat", host_str_concat)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "str_eq", host_str_eq)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "pow", host_pow)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arith_add", host_add)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arith_sub", host_sub)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arith_mul", host_mul)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arith_div", host_div)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arith_mod", host_mod)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arith_cmp", host_cmp)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arith_neg", host_neg)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arith_fneg", host_fneg)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "arr_load", host_arr_load)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "ffi_call_0", host_ffi_call_0)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "ffi_call_1", host_ffi_call_1)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "ffi_call_2", host_ffi_call_2)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "ffi_call_3", host_ffi_call_3)
            .map_err(map_wasmtime_err)?;
        linker
            .func_wrap("env", "ffi_call_4", host_ffi_call_4)
            .map_err(map_wasmtime_err)?;

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

        Ok(WasmRuntime {
            _engine: engine,
            store,
            init_func,
        })
    }

    /// Execute the module's `nulang_init` function, returning the tagged result.
    /// Set the input source for `IO.read` (used by tests; `IO.read` reads from
    /// stdin when no input is set).
    pub fn set_input(&mut self, input: &str) {
        let input_arc = self.store.data().input.clone();
        *input_arc.lock() = input.as_bytes().to_vec();
    }

    /// Set the effect-dispatch result (used by tests). The next
    /// `nulang_dispatch` call returns these bytes as the result written to
    /// the ring buffer; `None` (the default) returns length 0.
    pub fn set_dispatch_result(&mut self, result: Option<Vec<u8>>) {
        let arc = self.store.data().dispatch_result.clone();
        *arc.lock() = result;
    }

    /// Take the (tag, payload) pair from the last `nulang_dispatch` call,
    /// clearing it. Returns `None` if dispatch was never called.
    pub fn take_last_dispatch(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        self.store.data().last_dispatch.clone().lock().take()
    }

    pub fn run(&mut self) -> NuResult<crate::vm::Value> {
        let raw = self.init_func.call(&mut self.store, ()).map_err(|e| {
            // Host functions report interpreter-parity runtime errors (type
            // errors, 48-bit overflow) via `Error::msg`; wasmtime wraps them
            // in a trap whose Display only shows a backtrace. Downcast to
            // the original message and surface it as a RuntimeError so the
            // WASM backend agrees with the interpreter/JIT/AOT.
            if let Some(msg) = e.downcast_ref::<String>() {
                NuError::runtime_error(msg.clone(), Span::default())
            } else {
                map_wasmtime_err(e)
            }
        })?;
        Ok(crate::vm::Value::from_raw(raw as u64))
    }

    /// Resolve a tagged string `Value` (`TAG_STRING | offset`) to its text by
    /// reading the null-terminated bytes at that offset from linear memory.
    /// Returns `None` when the value is not a string or the offset is out of
    /// bounds. Used by tests and consumers that need concat/string content
    /// back out of a WASM execution.
    pub fn string_value(&self, val: &crate::vm::Value) -> Option<String> {
        use crate::value_layout::{PAYLOAD_MASK, TAG_MASK, TAG_STRING};
        let raw = val.as_raw();
        if (raw & TAG_MASK) != TAG_STRING {
            return None;
        }
        let offset = (raw & PAYLOAD_MASK) as usize;
        let mem = self.store.data().memory.as_ref()?;
        let data = mem.data(&self.store);
        let bytes: Vec<u8> = data
            .get(offset..)?
            .iter()
            .take_while(|&&b| b != 0)
            .copied()
            .collect();
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

// ── Host import functions ────────────────────────────────────────────

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
fn host_read(mut caller: Caller<'_, HostState>) -> Result<i64, Error> {
    // Read one line (mirrors the interpreter's IO.read), copy it into a
    // freshly bump-allocated WASM-memory buffer, and return it as a tagged
    // string. Nil on read error. Prefers the test input buffer over stdin.
    let bytes = {
        let input = caller.data().input.clone();
        let mut guard = input.lock();
        if guard.is_empty() {
            drop(guard);
            let mut s = String::new();
            if std::io::stdin().read_line(&mut s).is_err() {
                return Ok(value_layout::TAG_NIL as i64);
            }
            s.into_bytes()
        } else {
            let mut line = Vec::new();
            for &c in guard.iter() {
                line.push(c);
                if c == b'\n' {
                    break;
                }
            }
            guard.drain(..line.len());
            line
        }
    };
    let size = ((bytes.len() + 1) as u32 + 7) & !7u32; // align to 8
    let new_off = caller.data().alloc_offset;
    let required = new_off
        .checked_add(size)
        .ok_or_else(|| Error::msg("alloc overflow"))?;
    let mem = get_memory(&mut caller)?;
    if required > mem.data_size(&caller) as u32 {
        let pages_needed = ((required - mem.data_size(&caller) as u32) + 65535) / 65536;
        mem.grow(&mut caller, pages_needed as u64)
            .map_err(|e| Error::msg(format!("memory grow: {}", e)))?;
    }
    caller.data_mut().alloc_offset = required;

    let mem = get_memory(&mut caller)?;
    let data = mem.data_mut(&mut caller);
    let dst = new_off as usize;
    data[dst..dst + bytes.len()].copy_from_slice(&bytes);
    data[dst + bytes.len()] = 0;
    Ok(value_layout::TAG_STRING as i64 | new_off as i64)
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

/// `env.nulang_alloc(size: i32) -> i32`
///
/// Simple bump allocator in WASM linear memory. Single-threaded.
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

/// `env.str_concat(a: i64, b: i64) -> i64`
///
/// Concatenate two tagged string values. Each value is `TAG_STRING | offset`
/// into linear memory pointing at a null-terminated byte string (the data
/// segment is emitted with a trailing NUL per string, and prior concat
/// results are null-terminated here too). Reads both, writes `a ++ b\0` into
/// a fresh bump-allocated buffer, and returns the new tagged string value.
fn host_str_concat(mut caller: Caller<'_, HostState>, a: i64, b: i64) -> Result<i64, Error> {
    // Resolve each operand to its text, mirroring the interpreter's IAdd
    // string fallback (src/vm.rs): a tagged string reads its null-terminated
    // bytes from memory; anything else coerces through `to_string_repr()`, so
    // `"n=" + 42` concatenates the text "42".
    let (text_a, text_b) = {
        let mem = get_memory(&mut caller)?;
        let data = mem.data(&caller);
        let read = |v: i64| -> String {
            if (v as u64 & value_layout::TAG_MASK) == value_layout::TAG_STRING {
                let off = (v as u64 & value_layout::PAYLOAD_MASK) as usize;
                let bytes: Vec<u8> = data
                    .get(off..)
                    .map(|s| s.iter().take_while(|&&c| c != 0).copied().collect())
                    .unwrap_or_default();
                String::from_utf8_lossy(&bytes).into_owned()
            } else {
                crate::vm::Value::from_raw(v as u64).to_string_repr()
            }
        };
        (read(a), read(b))
    };
    let total = text_a.len() + text_b.len() + 1;
    // Bump-allocate (mirrors host_alloc) so the copy below can use `caller`.
    let size = (total as u32 + 7) & !7u32; // align to 8
    let new_off = caller.data().alloc_offset;
    let required = new_off
        .checked_add(size)
        .ok_or_else(|| Error::msg("alloc overflow"))?;
    let mem = get_memory(&mut caller)?;
    if required > mem.data_size(&caller) as u32 {
        let pages_needed = ((required - mem.data_size(&caller) as u32) + 65535) / 65536;
        mem.grow(&mut caller, pages_needed as u64)
            .map_err(|e| Error::msg(format!("memory grow: {}", e)))?;
    }
    caller.data_mut().alloc_offset = required;

    // Copy both texts into the freshly-allocated region, then null-terminate.
    let mem = get_memory(&mut caller)?;
    {
        let data = mem.data_mut(&mut caller);
        let dst = new_off as usize;
        data[dst..dst + text_a.len()].copy_from_slice(text_a.as_bytes());
        data[dst + text_a.len()..dst + text_a.len() + text_b.len()]
            .copy_from_slice(text_b.as_bytes());
        data[dst + text_a.len() + text_b.len()] = 0;
    }
    Ok(value_layout::TAG_STRING as i64 | new_off as i64)
}

/// `env.str_eq(a: i64, b: i64) -> i64`
///
/// String content equality: both operands must be tagged strings (read their
/// null-terminated bytes from memory); returns a tagged bool of whether they
/// hold the same text. Compares by content, not by data offset, so an
/// interned constant and a runtime `str_concat` result with identical text
/// compare equal. Returns `false` when either operand is not a string —
/// mirroring the interpreter's SCmpEq.
fn host_str_eq(mut caller: Caller<'_, HostState>, a: i64, b: i64) -> Result<i64, Error> {
    let eq = {
        let mem = get_memory(&mut caller)?;
        let data = mem.data(&caller);
        let read = |v: i64| -> Option<String> {
            if (v as u64 & value_layout::TAG_MASK) != value_layout::TAG_STRING {
                return None;
            }
            let off = (v as u64 & value_layout::PAYLOAD_MASK) as usize;
            let bytes: Vec<u8> = data
                .get(off..)
                .map(|s| s.iter().take_while(|&&c| c != 0).copied().collect())
                .unwrap_or_default();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        };
        match (read(a), read(b)) {
            (Some(sa), Some(sb)) => sa == sb,
            _ => false,
        }
    };
    Ok(value_layout::tag_bool(eq) as i64)
}

/// Dispatch an arithmetic op: when BOTH operands are floats (raw non-tag bit
/// patterns) do the f64 op; otherwise treat both as tagged ints. Mirrors the
/// interpreter's IAdd/ISub/IMul/IDiv/IMod semantics. The WASM backend has no
/// float arithmetic of its own (its `emit_binop` is integer-only), so numeric
/// ops route here.
fn host_arith_fi(a: u64, b: u64, fop: fn(f64, f64) -> f64, iop: fn(i64, i64) -> i64) -> i64 {
    if value_layout::is_float_raw(a) && value_layout::is_float_raw(b) {
        value_layout::float_bits(fop(f64::from_bits(a), f64::from_bits(b))) as i64
    } else {
        // Non-float, non-string operands (e.g. arrays) → 0, matching the
        // interpreter's `as_int().unwrap_or(0)`.
        let ia = crate::jit::runtime::as_int_or_zero(a);
        let ib = crate::jit::runtime::as_int_or_zero(b);
        value_layout::tag_int(iop(ia, ib)) as i64
    }
}

fn host_add(_caller: Caller<'_, HostState>, a: i64, b: i64) -> Result<i64, Error> {
    Ok(host_arith_fi(
        a as u64,
        b as u64,
        |x, y| x + y,
        |x, y| x + y,
    ))
}
fn host_sub(_caller: Caller<'_, HostState>, a: i64, b: i64) -> Result<i64, Error> {
    Ok(host_arith_fi(
        a as u64,
        b as u64,
        |x, y| x - y,
        |x, y| x - y,
    ))
}
fn host_mul(_caller: Caller<'_, HostState>, a: i64, b: i64) -> Result<i64, Error> {
    Ok(host_arith_fi(
        a as u64,
        b as u64,
        |x, y| x * y,
        |x, y| x.wrapping_mul(y),
    ))
}
fn host_div(_caller: Caller<'_, HostState>, a: i64, b: i64) -> Result<i64, Error> {
    let a = a as u64;
    let b = b as u64;
    if value_layout::is_float_raw(a) && value_layout::is_float_raw(b) {
        // Float division by zero → nil (matches the interpreter's IDiv).
        let denom = f64::from_bits(b);
        if denom == 0.0 {
            return Ok(value_layout::TAG_NIL as i64);
        }
        Ok(value_layout::float_bits(f64::from_bits(a) / denom) as i64)
    } else {
        let denom = crate::jit::runtime::as_int_or_one(b);
        if denom == 0 {
            return Ok(value_layout::TAG_NIL as i64);
        }
        Ok(value_layout::tag_int(crate::jit::runtime::as_int_or_zero(a) / denom) as i64)
    }
}
fn host_mod(_caller: Caller<'_, HostState>, a: i64, b: i64) -> Result<i64, Error> {
    let a = a as u64;
    let b = b as u64;
    if value_layout::is_float_raw(a) && value_layout::is_float_raw(b) {
        let denom = f64::from_bits(b);
        if denom == 0.0 {
            return Ok(value_layout::TAG_NIL as i64);
        }
        Ok(value_layout::float_bits(f64::from_bits(a) % denom) as i64)
    } else {
        let denom = crate::jit::runtime::as_int_or_one(b);
        if denom == 0 {
            return Ok(value_layout::TAG_NIL as i64);
        }
        Ok(value_layout::tag_int(crate::jit::runtime::as_int_or_zero(a) % denom) as i64)
    }
}

/// `env.arith_neg(a: i64) -> i64`
///
/// Unary negation: flip the sign bit for a float, negate the payload for an
/// int (matching the interpreter's INeg/FNeg). The WASM backend's inline
/// `UnOp::Neg` previously OR'd TAG_INT unconditionally, corrupting floats.
fn host_neg(_caller: Caller<'_, HostState>, a: i64) -> Result<i64, Error> {
    let a = a as u64;
    if value_layout::is_float_raw(a) {
        // Flip the IEEE-754 sign bit (bit 63), NOT `SIGN_BIT` (bit 47, the
        // 48-bit payload sign used for ints) — XORing the mantissa would
        // corrupt the float. Negating the canonical NaN flips it into the
        // tag range, so canonicalize the result.
        Ok(value_layout::float_bits(-f64::from_bits(a)) as i64)
    } else {
        // Match the interpreter's INeg (and the JIT helper `nulang_ineg`):
        // ints negate with a 48-bit overflow check at INT48_MIN; anything
        // else is a type error.
        let v = crate::vm::Value::from_raw(a);
        match v.as_int() {
            Some(x) if x != crate::value_layout::INT48_MIN => Ok(value_layout::tag_int(-x) as i64),
            Some(x) => Err(Error::msg(error_message(crate::vm::int_overflow_error(
                "neg", x, 0,
            )))),
            None => Err(Error::msg(error_message(crate::vm::arith_type_error(
                "neg", v, v,
            )))),
        }
    }
}

/// VM FNeg semantics: negate a real float, and use -0.0 for every tagged
/// or otherwise non-float operand via `as_float().unwrap_or(0.0)`.
fn host_fneg(_caller: Caller<'_, HostState>, a: i64) -> Result<i64, Error> {
    let a = a as u64;
    if value_layout::is_float_raw(a) {
        Ok(value_layout::float_bits(-f64::from_bits(a)) as i64)
    } else {
        Ok((-0.0f64).to_bits() as i64)
    }
}

/// Read a TAG_STRING value's null-terminated bytes from WASM linear memory.
fn read_wasm_string(mut caller: &mut Caller<'_, HostState>, v: i64) -> String {
    let v = v as u64;
    if (v & value_layout::TAG_MASK) != value_layout::TAG_STRING {
        return String::new();
    }
    let off = (v & value_layout::PAYLOAD_MASK) as usize;
    let data = match get_memory(&mut caller) {
        Ok(m) => m.data(&caller).to_vec(),
        Err(_) => return String::new(),
    };
    let bytes: Vec<u8> = data
        .get(off..)
        .map(|s| s.iter().take_while(|&&c| c != 0).copied().collect())
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `env.ffi_call_N(lib, sym, sig, arg0..argN-1) -> i64`
///
/// Invoke a foreign C function from WASM. `lib`/`sym` are TAG_STRING constants
/// interned into the WASM data segment; `sig` is the bit-packed CType signature
/// (low 3 bits = return tag, then 3 bits per param, matching the AOT backend).
/// CStr params are read from WASM memory. Reuses the AOT FFI registry + marshalling.
fn host_ffi_call_impl(
    mut caller: &mut Caller<'_, HostState>,
    lib: i64,
    sym: i64,
    sig: i64,
    args: &[i64],
) -> Result<i64, Error> {
    let lib_s = read_wasm_string(&mut caller, lib);
    let sym_s = read_wasm_string(&mut caller, sym);
    let sig = sig as u64;
    let ret_tag = sig & 0b111;
    let mut params: Vec<crate::ffi::marshal::CType> = Vec::with_capacity(args.len());
    for i in 0..args.len() {
        let tag = (sig >> (3 + 3 * i as u32)) & 0b111;
        params.push(crate::jit::runtime::aot_ctype_from_tag(tag));
    }
    let ret = crate::jit::runtime::aot_ctype_from_tag(ret_tag);
    let signature = crate::ffi::marshal::Signature::new(params.clone(), ret);
    let func = {
        let registry = crate::ffi::native::FFI_REGISTRY
            .get_or_init(|| std::sync::Mutex::new(crate::ffi::native::FfiRegistry::new()));
        let mut reg = registry
            .lock()
            .map_err(|_| Error::msg("ffi registry lock"))?;
        unsafe { reg.resolve_or_load(&lib_s, &sym_s, signature) }
            .map_err(|_| Error::msg("ffi resolve"))?
    };
    // Marshal CStr params from WASM memory into CStrings valid for the call.
    let mut cstrings: Vec<std::ffi::CString> = Vec::new();
    let mut cargs: Vec<crate::vm::Value> = Vec::with_capacity(args.len());
    for (i, p) in params.iter().enumerate() {
        if *p == crate::ffi::marshal::CType::CStr {
            let s = read_wasm_string(&mut caller, args[i]);
            let c = std::ffi::CString::new(s).map_err(|_| Error::msg("bad cstr"))?;
            cargs.push(crate::vm::Value::ptr(c.as_ptr() as *mut u8));
            cstrings.push(c);
        } else {
            cargs.push(crate::vm::Value::from_bits(args[i] as u64));
        }
    }
    // SAFETY: func.ptr points to a function whose ABI matches the signature.
    match unsafe { crate::ffi::marshal::call_native(&func, &cargs) } {
        Ok(v) => Ok(v.as_raw() as i64),
        Err(_) => Ok(value_layout::TAG_NIL as i64),
    }
}

macro_rules! define_wasm_ffi_call {
    ($name:ident, $($arg:ident),*) => {
        fn $name(mut caller: Caller<'_, HostState>, lib: i64, sym: i64, sig: i64 $(, $arg: i64)*) -> Result<i64, Error> {
            let args = [$($arg),*];
            host_ffi_call_impl(&mut caller, lib, sym, sig, &args)
        }
    };
}

define_wasm_ffi_call!(host_ffi_call_0,);
define_wasm_ffi_call!(host_ffi_call_1, a0);
define_wasm_ffi_call!(host_ffi_call_2, a0, a1);
define_wasm_ffi_call!(host_ffi_call_3, a0, a1, a2);
define_wasm_ffi_call!(host_ffi_call_4, a0, a1, a2, a3);

/// `env.arr_load(arr: i64, idx: i64) -> i64`
///
/// Array element load with bounds check. `arr` is a TAG_PTR to a heap block
/// `[count][elem0]..`; reads element `idx` or nil when out of range (matching
/// the interpreter). Negative indices become huge after payload masking → OOB.
fn host_arr_load(mut caller: Caller<'_, HostState>, arr: i64, idx: i64) -> Result<i64, Error> {
    let arr = arr as u64;
    let base = (arr & value_layout::PAYLOAD_MASK) as usize;
    let idx = (idx as u64 & value_layout::PAYLOAD_MASK) as usize;
    let mem = get_memory(&mut caller)?;
    let data = mem.data(&caller);
    let read = |off: usize| -> u64 {
        data.get(off..)
            .map(|s| {
                let mut b = [0u8; 8];
                b.copy_from_slice(&s[..8.min(s.len())]);
                u64::from_le_bytes(b)
            })
            .unwrap_or(0)
    };
    let count = read(base) as usize;
    if idx >= count {
        return Ok(value_layout::TAG_NIL as i64);
    }
    Ok(read(base + (idx + 1) * 8) as i64)
}

/// `env.arith_cmp(a: i64, b: i64, code: i64) -> i64`
///
/// Compare two values (float when both are floats, else signed int). `code`:
/// 0=Eq, 1=Ne, 2=Lt, 3=Gt, 4=Le, 5=Ge. Returns a tagged bool.
fn host_cmp(_caller: Caller<'_, HostState>, a: i64, b: i64, code: i64) -> Result<i64, Error> {
    let a = a as u64;
    let b = b as u64;
    let (fa, fb) = (f64::from_bits(a), f64::from_bits(b));
    let (ia, ib) = (
        value_layout::sext48(a & value_layout::PAYLOAD_MASK),
        value_layout::sext48(b & value_layout::PAYLOAD_MASK),
    );
    let eq = if value_layout::is_float_raw(a) && value_layout::is_float_raw(b) {
        match code {
            0 => fa == fb,
            1 => fa != fb,
            2 => fa < fb,
            3 => fa > fb,
            4 => fa <= fb,
            _ => fa >= fb,
        }
    } else {
        match code {
            0 => ia == ib,
            1 => ia != ib,
            2 => ia < ib,
            3 => ia > ib,
            4 => ia <= ib,
            _ => ia >= ib,
        }
    };
    // Comparisons always produce booleans, including when both operands are
    // floats. Returning a float 0.0/1.0 makes `as_bool()` fail and diverges
    // from the interpreter's FCmp* opcodes.
    Ok(value_layout::tag_bool(eq) as i64)
}

/// `env.pow(a: i64, b: i64) -> i64`
///
/// Integer exponentiation `a ** b` for tagged integer values. Mirrors the
/// interpreter/AOT `nulang_pow`: negative exponent or overflow → nil; 0^0 = 1.
fn host_pow(_caller: Caller<'_, HostState>, a: i64, b: i64) -> Result<i64, Error> {
    let a = a as u64;
    let b = b as u64;
    // Match the interpreter: both floats → powf; else int pow with
    // wrapping_mul (negative exponent → nil).
    if value_layout::is_float_raw(a) && value_layout::is_float_raw(b) {
        return Ok(value_layout::float_bits(f64::from_bits(a).powf(f64::from_bits(b))) as i64);
    }
    let base = crate::jit::runtime::as_int_or_zero(a);
    let exp = crate::jit::runtime::as_int_or_zero(b);
    if exp < 0 {
        return Ok(value_layout::TAG_NIL as i64);
    }
    let mut result: i64 = 1;
    let mut base = base;
    let mut exp = exp;
    while exp > 0 {
        if exp & 1 != 0 {
            result = result.wrapping_mul(base);
        }
        exp >>= 1;
        if exp > 0 {
            base = base.wrapping_mul(base);
        }
    }
    Ok(value_layout::tag_int(result) as i64)
}

/// `env.nulang_dispatch(a: i32, b: i32, c: i32, d: i32) -> i64`
///
/// Effect dispatch stub: writes the injectable [`HostState::dispatch_result`]
/// (if any) to the ring buffer at [`crate::mir_wasm::RING_BUFFER_BASE`] and
/// returns its length — matching the length-return contract the pool's
/// `host_dispatch` implements (the wasmtime-actor-pool bridges the real
/// dispatch to `EffectRuntimePool`). Returns 0 when no result is injected.
fn host_dispatch(mut caller: Caller<'_, HostState>, a: i32, b: i32, c: i32, d: i32) -> i64 {
    // Record the (tag, payload) pair for test verification of the
    // compiler's marshaling. Scoped to release the memory borrow before the
    // result write-back below takes `&mut caller`.
    {
        let mem = match get_memory(&mut caller) {
            Ok(m) => m,
            Err(_) => return 0,
        };
        let data = mem.data(&caller);
        let read = |off: i32, len: i32| -> Vec<u8> {
            if off < 0 || len <= 0 {
                return Vec::new();
            }
            let (off, len) = (off as usize, len as usize);
            data.get(off..off.saturating_add(len))
                .unwrap_or(&[])
                .to_vec()
        };
        *caller.data().last_dispatch.lock() = Some((read(a, b), read(c, d)));
    }
    let result = {
        let guard = caller.data().dispatch_result.lock();
        guard.clone()
    };
    let Some(result) = result else {
        return 0;
    };
    if result.is_empty() {
        return 0;
    }
    let base = crate::mir_wasm::RING_BUFFER_BASE as usize;
    let write_len = result.len().min(0x1000); // ring buffer is 4 KiB
    let mem = match get_memory(&mut caller) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let data = mem.data_mut(&mut caller);
    if base + write_len > data.len() {
        return 0;
    }
    data[base..base + write_len].copy_from_slice(&result[..write_len]);
    write_len as i64
}

/// Decode one tagged Nulang value (as produced by the WASM backend) into a
/// plain JSON value. `TAG_STRING` payload is an offset into `data` addressing
/// NUL-terminated UTF-8; out-of-bounds or unterminated → null. Untransferable
/// tags (ptr/actor/closure) → null. Unreadable float bits → null.
fn guest_value_to_json(raw: u64, data: &[u8]) -> serde_json::Value {
    use value_layout as vl;
    if vl::is_float_raw(raw) {
        return serde_json::Number::from_f64(f64::from_bits(raw))
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
    }
    match raw & vl::TAG_MASK {
        vl::TAG_NIL | vl::TAG_UNIT => serde_json::Value::Null,
        vl::TAG_BOOL => serde_json::Value::Bool(raw & 1 != 0),
        vl::TAG_INT => serde_json::Value::from(vl::as_int_raw(raw)),
        vl::TAG_STRING => {
            let off = (raw & vl::PAYLOAD_MASK) as usize;
            if off >= data.len() {
                return serde_json::Value::Null;
            }
            match data[off..].iter().position(|&b| b == 0) {
                Some(n) => serde_json::Value::String(
                    String::from_utf8_lossy(&data[off..off + n]).into_owned(),
                ),
                None => serde_json::Value::Null,
            }
        }
        vl::TAG_PTR | vl::TAG_ACTOR | vl::TAG_CLOSURE => serde_json::Value::Null,
        _ => serde_json::Value::Null,
    }
}

/// `env.nulang_dispatch_args(tag_ptr: i32, tag_len: i32, argv_ptr: i32, argc: i32) -> i64`
///
/// Runtime-argument effect dispatch stub: decodes the guest's positional argv
/// (tagged Nulang values in linear memory) into a positional JSON array,
/// records it alongside the dotted effect tag for test verification, and
/// writes the injectable [`HostState::dispatch_result`] (if any) to the ring
/// buffer at [`crate::mir_wasm::RING_BUFFER_BASE`], returning its length —
/// matching the length-return contract nulang-cloud's `host_dispatch_args`
/// implements. Returns 0 when no result is injected.
fn host_dispatch_args(
    mut caller: Caller<'_, HostState>,
    tag_ptr: i32,
    tag_len: i32,
    argv_ptr: i32,
    argc: i32,
) -> i64 {
    const MAX_DISPATCH_ARGS: i32 = 16;
    if argc < 0 || argc > MAX_DISPATCH_ARGS {
        return 0;
    }
    // Read the tag + argv words, decode into a positional JSON array, and
    // record for tests. Scoped to release the memory borrow before the result
    // write-back below takes `&mut caller`.
    {
        let mem = match get_memory(&mut caller) {
            Ok(m) => m,
            Err(_) => return 0,
        };
        let data = mem.data(&caller);
        let read = |off: i32, len: i32| -> Vec<u8> {
            if off < 0 || len <= 0 {
                return Vec::new();
            }
            let (off, len) = (off as usize, len as usize);
            data.get(off..off.saturating_add(len))
                .unwrap_or(&[])
                .to_vec()
        };
        let tag = read(tag_ptr, tag_len);
        let argv = read(argv_ptr, argc * 8);
        let args: Vec<serde_json::Value> = argv
            .chunks_exact(8)
            .map(|w| {
                let raw = u64::from_le_bytes(w.try_into().expect("8-byte chunk"));
                guest_value_to_json(raw, data)
            })
            .collect();
        let payload = serde_json::to_vec(&serde_json::Value::Array(args)).unwrap_or_default();
        *caller.data().last_dispatch.lock() = Some((tag, payload));
    }
    let result = {
        let guard = caller.data().dispatch_result.lock();
        guard.clone()
    };
    let Some(result) = result else {
        return 0;
    };
    if result.is_empty() {
        return 0;
    }
    let base = crate::mir_wasm::RING_BUFFER_BASE as usize;
    let write_len = result.len().min(0x1000); // ring buffer is 4 KiB
    let mem = match get_memory(&mut caller) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let data = mem.data_mut(&mut caller);
    if base + write_len > data.len() {
        return 0;
    }
    data[base..base + write_len].copy_from_slice(&result[..write_len]);
    write_len as i64
}

/// Helper: retrieve linear memory from the HostState.
fn get_memory(caller: &mut Caller<'_, HostState>) -> Result<Memory, Error> {
    caller
        .data()
        .memory
        .clone()
        .ok_or_else(|| Error::msg("env.memory not initialized"))
}

// ── Error mapping ────────────────────────────────────────────────────

/// Extract the raw message from a `NuError::RuntimeError` (the message
/// without the "Runtime error at L:C:" Display prefix). Host functions use
/// this so the error re-wrapped by `WasmRuntime::run` matches the
/// interpreter's error text exactly.
fn error_message(e: NuError) -> String {
    match e {
        NuError::RuntimeError { msg, .. } => msg,
        other => other.to_string(),
    }
}

fn map_wasmtime_err(e: impl std::fmt::Display) -> NuError {
    NuError::VMError {
        msg: format!("wasmtime: {}", e),
        span: Span::default(),
    }
}

// ── AOT compilation ──────────────────────────────────────────────────

/// Compile a WASM module ahead-of-time to a `.cwasm` file via `wasmtime compile`.
/// Compile a WebAssembly module to a machine-specific `.cwasm` artifact.
/// Note: No cross-version portability is promised for `.cwasm`. It must be
/// loaded by an `Engine` matching this version of wasmtime and its config.
pub fn aot_compile(wasm_path: &str, cwasm_path: &str) -> NuResult<()> {
    let bytes = std::fs::read(wasm_path).map_err(|e| NuError::VMError {
        msg: format!("failed to read wasm: {}", e),
        span: Span::default(),
    })?;

    let config = default_wasm_config();
    let engine = Engine::new(&config).map_err(|e| NuError::VMError {
        msg: format!("failed to create engine: {}", e),
        span: Span::default(),
    })?;

    let cwasm_bytes = engine
        .precompile_module(&bytes)
        .map_err(|e| NuError::VMError {
            msg: format!("failed to precompile module: {}", e),
            span: Span::default(),
        })?;

    std::fs::write(cwasm_path, cwasm_bytes).map_err(|e| NuError::VMError {
        msg: format!("failed to write cwasm: {}", e),
        span: Span::default(),
    })?;

    Ok(())
}

/// Load a precompiled `.cwasm` module and instantiate it.
pub fn load_precompiled(cwasm_bytes: &[u8]) -> NuResult<WasmRuntime> {
    let config = default_wasm_config();
    let engine = Engine::new(&config).map_err(map_wasmtime_err)?;

    let module = unsafe { Module::deserialize(&engine, cwasm_bytes) }.map_err(map_wasmtime_err)?;

    let mut store = Store::new(&engine, HostState::default());
    let mut linker: Linker<HostState> = Linker::new(&engine);

    linker
        .func_wrap("env", "nulang_alloc", host_alloc)
        .map_err(map_wasmtime_err)?;
    linker
        .func_wrap("env", "nulang_dispatch", host_dispatch)
        .map_err(map_wasmtime_err)?;
    linker
        .func_wrap("env", "nulang_dispatch_args", host_dispatch_args)
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

    let mem_type = MemoryType::new(1, None);
    let memory = Memory::new(&mut store, mem_type).map_err(map_wasmtime_err)?;
    linker
        .define(&mut store, "env", "memory", memory)
        .map_err(map_wasmtime_err)?;

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(map_wasmtime_err)?;

    if let Some(exported_mem) = instance.get_memory(&mut store, "memory") {
        let data_end = exported_mem.data_size(&store);
        store.data_mut().alloc_offset = data_end as u32;
    }

    let init_func = instance
        .get_typed_func::<(), i64>(&mut store, "nulang_init")
        .map_err(map_wasmtime_err)?;

    Ok(WasmRuntime {
        _engine: engine,
        store,
        init_func,
    })
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_creates() {
        let config = default_wasm_config();
        let engine = Engine::new(&config);
        assert!(engine.is_ok(), "engine should create: {:?}", engine.err());
    }

    #[test]
    fn test_wasm_runtime_empty_module() {
        // Minimal valid WASM module: magic + version.
        let wasm = vec![
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
        ];
        let config = default_wasm_config();
        let engine = Engine::new(&config).unwrap();
        assert!(Module::new(&engine, &wasm).is_ok());
    }

    #[test]
    fn test_wasm_config_reservation_sizes() {
        let config = default_wasm_config();
        let engine = Engine::new(&config).unwrap();
        // Verify default config settings don't conflict.
        let module = Module::new(&engine, &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
        assert!(module.is_ok());
    }

    #[test]
    fn test_aot_compile_rejects_missing_file() {
        let result = aot_compile("/nonexistent/path.wasm", "/tmp/out.cwasm");
        assert!(result.is_err(), "compiling a missing file should fail");
    }

    #[test]
    fn test_error_mapping() {
        let err = map_wasmtime_err("test error");
        assert!(err.to_string().contains("wasmtime"));
        assert!(err.to_string().contains("test error"));
    }

    #[test]
    fn test_wasm_runtime_rejects_invalid_module() {
        let config = default_wasm_config();
        let engine = Engine::new(&config).unwrap();
        let invalid_wasm = vec![0x00, 0x00, 0x00, 0x00];
        let result = Module::new(&engine, &invalid_wasm);
        assert!(result.is_err(), "invalid WASM should fail to parse");
    }

    #[test]
    fn test_wasm_runtime_rejects_empty_bytes() {
        let config = default_wasm_config();
        let engine = Engine::new(&config).unwrap();
        let result = Module::new(&engine, &[] as &[u8]);
        assert!(result.is_err(), "empty bytes should fail to parse");
    }

    #[test]
    fn test_host_read_reads_input() {
        let wasm = br#"(module
            (import "env" "memory" (memory 1))
            (import "env" "nulang_alloc" (func $alloc (param i32) (result i32)))
            (import "env" "nulang_dispatch" (func $dispatch (param i32 i32 i32 i32) (result i64)))
            (import "env" "log" (func $log (param i32 i32) (result i64)))
            (import "env" "io_print" (func $print (param i32 i32) (result i64)))
            (import "env" "io_read" (func $read (result i64)))
            (func $start (result i64)
                call $read
            )
            (export "nulang_init" (func $start))
        )"#;
        let mut runtime = WasmRuntime::new(wasm, None).unwrap();
        runtime.set_input("hello from input\n");
        let result = runtime.run().unwrap();
        let s = runtime.string_value(&result);
        assert_eq!(
            s.as_deref(),
            Some("hello from input\n"),
            "io_read must read from the input source"
        );
    }
}
