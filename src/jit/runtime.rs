//! Runtime helper functions callable from JIT-compiled code.

use crate::bytecode::Constant;
use crate::value_layout::{
    is_float_raw, sext48, tag_int, INT48_MAX, INT48_MIN, PAYLOAD_MASK, TAG_CLOSURE, TAG_INT,
    TAG_MASK, TAG_PTR, TAG_STRING,
};
use crate::vm::Value;
use std::cell::{Cell, UnsafeCell};
// is_float_raw is now imported from crate::value_layout (integer bitmask, no FPU).

/// Coerce a raw Nulang value to its string representation: the string content
/// if it IS a string (constant-pool or heap), otherwise `Value::to_string_repr`
/// (matching the interpreter's IAdd string fallback, so `"n=" + 42 == "n=42"`).
fn coerce_string(raw: u64) -> String {
    match resolve_string_coerce(raw) {
        Some(s) => s,
        None => Value::from_raw(raw).to_string_repr(),
    }
}

/// Is the value ACTUALLY a string (a TAG_STRING constant or a TAG_PTR heap
/// string), as opposed to merely coercible to one? `resolve_string_coerce`
/// returns Some for ints/floats/bools too, so it can't gate the concat path.
fn raw_is_string(raw: u64) -> bool {
    let val = Value::from_raw(raw);
    if val.is_string() {
        return true;
    }
    if (raw & TAG_MASK) == TAG_PTR {
        let ptr = (raw & PAYLOAD_MASK) as *mut u8;
        if ptr.is_null() {
            return false;
        }
        // SAFETY: TAG_PTR values reaching the JIT runtime are produced by
        // `alloc_obj` on this thread's actor heap, so `ptr` is a live
        // payload pointer and `header_of` recovers its valid header.
        unsafe {
            let header = &*ActorHeap::header_of(ptr);
            header.type_tag == HeapTypeTag::String
        }
    } else {
        false
    }
}

/// Allocate a heap string holding `s` and return its tagged pointer value.
fn alloc_string_value(s: String) -> u64 {
    let bytes = s.into_bytes();
    // SAFETY: `alloc_obj` returns a fresh payload of `bytes.len() + 1`
    // bytes, so the copy plus trailing NUL fit exactly. The NUL satisfies
    // the heap-string null-termination invariant relied on by
    // `heap_string_payload` and other string readers.
    unsafe {
        if let Some(ptr) = alloc_obj(bytes.len() + 1, HeapTypeTag::String) {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            *ptr.add(bytes.len()) = 0;
            debug_assert!(
                crate::value_layout::ptr_fits_payload(ptr as u64),
                "heap pointer exceeds 48-bit value payload; address would be truncated"
            );
            Value::ptr(ptr).as_raw()
        } else {
            Value::nil().as_raw()
        }
    }
}

#[no_mangle]
pub extern "C" fn nulang_iadd(a: u64, b: u64) -> u64 {
    // String concatenation fallback (mirrors the interpreter's IAdd): when the
    // compiler couldn't determine operand types at compile time (e.g. a string
    // added to an int), coerce both operands to strings and concatenate.
    if raw_is_string(a) || raw_is_string(b) {
        let result = format!("{}{}", coerce_string(a), coerce_string(b));
        return alloc_string_value(result);
    }
    if is_float_raw(a) && is_float_raw(b) {
        Value::float(f64::from_bits(a) + f64::from_bits(b)).as_raw()
    } else {
        tag_int(as_int_or_zero(a) + as_int_or_zero(b))
    }
}

#[no_mangle]
pub extern "C" fn nulang_isub(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        Value::float(f64::from_bits(a) - f64::from_bits(b)).as_raw()
    } else {
        tag_int(as_int_or_zero(a) - as_int_or_zero(b))
    }
}

#[no_mangle]
pub extern "C" fn nulang_imul(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        Value::float(f64::from_bits(a) * f64::from_bits(b)).as_raw()
    } else {
        tag_int(as_int_or_zero(a).wrapping_mul(as_int_or_zero(b)))
    }
}

#[no_mangle]
pub extern "C" fn nulang_idiv(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        let bv = f64::from_bits(b);
        if bv == 0.0 {
            return Value::nil().as_raw();
        }
        return Value::float(f64::from_bits(a) / bv).as_raw();
    }
    let bv = as_int_or_one(b);
    if bv == 0 {
        return Value::nil().as_raw();
    }
    tag_int(as_int_or_zero(a) / bv)
}

#[no_mangle]
pub extern "C" fn nulang_imod(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        let bv = f64::from_bits(b);
        if bv == 0.0 {
            return Value::nil().as_raw();
        }
        return Value::float(f64::from_bits(a) % bv).as_raw();
    }
    let bv = as_int_or_one(b);
    if bv == 0 {
        return Value::nil().as_raw();
    }
    tag_int(as_int_or_zero(a) % bv)
}

/// Denominator for div/mod, matching the interpreter's `as_int().unwrap_or(1)`:
/// a non-int-tagged denominator is 1 (no div-by-zero), while a tagged int 0
/// still yields div-by-zero → nil.
pub(crate) fn as_int_or_one(v: u64) -> i64 {
    if (v & TAG_MASK) == TAG_INT {
        sext48(v & PAYLOAD_MASK)
    } else {
        1
    }
}

/// Extract the integer payload like the interpreter's `as_int().unwrap_or(0)`:
/// non-int-tagged values contribute 0.
pub(crate) fn as_int_or_zero(v: u64) -> i64 {
    if (v & TAG_MASK) == TAG_INT {
        sext48(v & PAYLOAD_MASK)
    } else {
        0
    }
}

/// Extract the raw payload pointer from a NaN-boxed value, or null.
fn val_ptr(v: u64) -> *mut u8 {
    if (v & TAG_MASK) == TAG_PTR {
        (v & PAYLOAD_MASK) as *mut u8
    } else {
        std::ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn nulang_xor(a: u64, b: u64) -> u64 {
    tag_int(as_int_or_zero(a) ^ as_int_or_zero(b))
}

#[no_mangle]
pub extern "C" fn nulang_shl(a: u64, b: u64) -> u64 {
    let shift = (as_int_or_zero(b) as u64) & 0x3f;
    tag_int(as_int_or_zero(a) << shift)
}

#[no_mangle]
pub extern "C" fn nulang_shr(a: u64, b: u64) -> u64 {
    let shift = (as_int_or_zero(b) as u64) & 0x3f;
    tag_int(as_int_or_zero(a) >> shift)
}

#[no_mangle]
pub extern "C" fn nulang_bitand(a: u64, b: u64) -> u64 {
    tag_int(as_int_or_zero(a) & as_int_or_zero(b))
}

#[no_mangle]
pub extern "C" fn nulang_bitor(a: u64, b: u64) -> u64 {
    tag_int(as_int_or_zero(a) | as_int_or_zero(b))
}

#[no_mangle]
pub extern "C" fn nulang_ineg(a: u64) -> u64 {
    // Match the interpreter's INeg: floats negate; ints negate with a
    // 48-bit overflow check at INT48_MIN; anything else is a type error.
    if is_float_raw(a) {
        Value::float(-f64::from_bits(a)).as_raw()
    } else {
        let v = Value::from_raw(a);
        match v.as_int() {
            Some(x) if x != INT48_MIN => Value::int(-x).as_raw(),
            Some(x) => record_arith_error(crate::vm::int_overflow_error("neg", x, 0)),
            None => record_arith_error(crate::vm::arith_type_error("neg", v, v)),
        }
    }
}

/// Record an arithmetic runtime error for the AOT driver and yield nil
/// (compiled code cannot unwind; see `AOT_PENDING_ERROR`).
fn record_arith_error(e: crate::types::NuError) -> u64 {
    let msg = match e {
        crate::types::NuError::RuntimeError { msg, .. } => msg,
        other => other.to_string(),
    };
    aot_set_pending_error(msg);
    Value::nil().as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_iinc(a: u64) -> u64 {
    // IInc/IDec read the raw 48-bit payload as a signed value (tag ignored),
    // matching step_iinc — NOT as_int_or_zero (which would zero non-int tags).
    tag_int(sext48(a & PAYLOAD_MASK) + 1)
}

#[no_mangle]
pub extern "C" fn nulang_idec(a: u64) -> u64 {
    tag_int(sext48(a & PAYLOAD_MASK) - 1)
}

#[no_mangle]
pub extern "C" fn nulang_icmp_eq(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        Value::bool((f64::from_bits(a) - f64::from_bits(b)).abs() < f64::EPSILON).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT {
        Value::bool(sext48(a & PAYLOAD_MASK) == sext48(b & PAYLOAD_MASK)).as_raw()
    } else if is_float_raw(a) && (b & TAG_MASK) == TAG_INT {
        let bf = sext48(b & PAYLOAD_MASK) as f64;
        Value::bool((f64::from_bits(a) - bf).abs() < f64::EPSILON).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && is_float_raw(b) {
        let af = sext48(a & PAYLOAD_MASK) as f64;
        Value::bool((af - f64::from_bits(b)).abs() < f64::EPSILON).as_raw()
    } else if (a & TAG_MASK) == TAG_STRING
        || (a & TAG_MASK) == TAG_PTR
        || (b & TAG_MASK) == TAG_STRING
        || (b & TAG_MASK) == TAG_PTR
    {
        // String equality must compare content, not raw bits.
        // Only when BOTH resolve to strings do we compare text.
        let eq = match (resolve_jit_string(a), resolve_jit_string(b)) {
            (Some(sa), Some(sb)) => sa == sb,
            _ => false,
        };
        Value::bool(eq).as_raw()
    } else {
        Value::bool(a == b).as_raw()
    }
}

#[no_mangle]
pub extern "C" fn nulang_icmp_lt(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        Value::bool(f64::from_bits(a) < f64::from_bits(b)).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT {
        Value::bool(sext48(a & PAYLOAD_MASK) < sext48(b & PAYLOAD_MASK)).as_raw()
    } else if is_float_raw(a) && (b & TAG_MASK) == TAG_INT {
        Value::bool(f64::from_bits(a) < sext48(b & PAYLOAD_MASK) as f64).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && is_float_raw(b) {
        Value::bool((sext48(a & PAYLOAD_MASK) as f64) < f64::from_bits(b)).as_raw()
    } else {
        Value::bool(a < b).as_raw()
    }
}

#[no_mangle]
pub extern "C" fn nulang_icmp_gt(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        Value::bool(f64::from_bits(a) > f64::from_bits(b)).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT {
        Value::bool(sext48(a & PAYLOAD_MASK) > sext48(b & PAYLOAD_MASK)).as_raw()
    } else if is_float_raw(a) && (b & TAG_MASK) == TAG_INT {
        Value::bool(f64::from_bits(a) > sext48(b & PAYLOAD_MASK) as f64).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && is_float_raw(b) {
        Value::bool((sext48(a & PAYLOAD_MASK) as f64) > f64::from_bits(b)).as_raw()
    } else {
        Value::bool(a > b).as_raw()
    }
}

#[no_mangle]
pub extern "C" fn nulang_icmp_le(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        Value::bool(f64::from_bits(a) <= f64::from_bits(b)).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT {
        Value::bool(sext48(a & PAYLOAD_MASK) <= sext48(b & PAYLOAD_MASK)).as_raw()
    } else if is_float_raw(a) && (b & TAG_MASK) == TAG_INT {
        Value::bool(f64::from_bits(a) <= sext48(b & PAYLOAD_MASK) as f64).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && is_float_raw(b) {
        Value::bool((sext48(a & PAYLOAD_MASK) as f64) <= f64::from_bits(b)).as_raw()
    } else {
        Value::bool(a <= b).as_raw()
    }
}

#[no_mangle]
pub extern "C" fn nulang_icmp_ge(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        Value::bool(f64::from_bits(a) >= f64::from_bits(b)).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT {
        Value::bool(sext48(a & PAYLOAD_MASK) >= sext48(b & PAYLOAD_MASK)).as_raw()
    } else if is_float_raw(a) && (b & TAG_MASK) == TAG_INT {
        Value::bool(f64::from_bits(a) >= sext48(b & PAYLOAD_MASK) as f64).as_raw()
    } else if (a & TAG_MASK) == TAG_INT && is_float_raw(b) {
        Value::bool((sext48(a & PAYLOAD_MASK) as f64) >= f64::from_bits(b)).as_raw()
    } else {
        Value::bool(a >= b).as_raw()
    }
}

#[no_mangle]
pub extern "C" fn nulang_fadd(a: u64, b: u64) -> u64 {
    Value::float(f64::from_bits(a) + f64::from_bits(b)).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_fsub(a: u64, b: u64) -> u64 {
    Value::float(f64::from_bits(a) - f64::from_bits(b)).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_fmul(a: u64, b: u64) -> u64 {
    Value::float(f64::from_bits(a) * f64::from_bits(b)).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_fdiv(a: u64, b: u64) -> u64 {
    let bv = f64::from_bits(b);
    if bv == 0.0 {
        return Value::nil().as_raw();
    }
    Value::float(f64::from_bits(a) / bv).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_fcmp_eq(a: u64, b: u64) -> u64 {
    Value::bool((f64::from_bits(a) - f64::from_bits(b)).abs() < f64::EPSILON).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_fcmp_lt(a: u64, b: u64) -> u64 {
    Value::bool(f64::from_bits(a) < f64::from_bits(b)).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_fcmp_gt(a: u64, b: u64) -> u64 {
    Value::bool(f64::from_bits(a) > f64::from_bits(b)).as_raw()
}

fn is_truthy(v: u64) -> bool {
    v != Value::nil().as_raw() && v != Value::bool(false).as_raw() && v != Value::int(0).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_not(a: u64) -> u64 {
    Value::bool(is_truthy(a) == false).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_and(a: u64, b: u64) -> u64 {
    Value::bool(is_truthy(a) && is_truthy(b)).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_or(a: u64, b: u64) -> u64 {
    Value::bool(is_truthy(a) || is_truthy(b)).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_itof(a: u64) -> u64 {
    Value::float(sext48(a & PAYLOAD_MASK) as f64).as_raw()
}

#[no_mangle]
pub extern "C" fn nulang_ftoi(a: u64) -> u64 {
    // `as i64` saturates; clamp to the 48-bit payload range (see Value::int).
    let n = (f64::from_bits(a) as i64).clamp(INT48_MIN, INT48_MAX);
    Value::int(n).as_raw()
}

/// Float negate, matching the interpreter's `FNeg`: floats negate (NaN stays
/// NaN, canonicalized); any tagged (non-float) value maps to -0.0.
#[no_mangle]
pub extern "C" fn nulang_fneg(a: u64) -> u64 {
    let f = f64::from_bits(a);
    let v = if is_float_raw(a) { f } else { 0.0 };
    Value::float(-v).as_raw()
}

// -----------------------------------------------------------------------
// Actor callback thread-local for JIT runtime helpers
// -----------------------------------------------------------------------

/// Raw pair representing a `*mut dyn ActorVmCallbacks` fat pointer.
/// Stored as two usize values to avoid zero-initialization UB.
#[derive(Clone, Copy)]
struct CbPair(usize, usize);

impl CbPair {
    const NULL: Self = CbPair(0, 0);

    /// # Safety
    /// Transmutes `*mut dyn ActorVmCallbacks` (a fat pointer: data ptr +
    /// vtable ptr) to `(usize, usize)`. Relies on the de-facto fat pointer
    /// layout used by all Tier-1 Rust targets (x86_64, aarch64).
    fn from_ptr(ptr: *mut dyn crate::vm::ActorVmCallbacks) -> Self {
        unsafe { std::mem::transmute(ptr) }
    }

    /// # Safety
    /// Reconstructs the fat pointer. The caller must ensure the original
    /// `&mut dyn ActorVmCallbacks` is alive and `&mut` provenance restored.
    fn to_ptr(self) -> *mut dyn crate::vm::ActorVmCallbacks {
        unsafe { std::mem::transmute(self) }
    }

    fn is_null(self) -> bool {
        self.0 == 0 && self.1 == 0
    }
}

thread_local! {
    static JIT_CALLBACKS: UnsafeCell<CbPair> = UnsafeCell::new(CbPair::NULL);
}

thread_local! {
    /// Pending runtime-error message set by arithmetic helpers (pow/neg/...)
    /// when the interpreter would raise (48-bit overflow, type error).
    /// JIT-compiled code cannot unwind, so the helper records the error here
    /// and returns nil; `AotModule::run` (which CAN return an error) checks
    /// this after the compiled entry point returns, keeping the AOT backend
    /// in lockstep with the interpreter's checked arithmetic.
    static AOT_PENDING_ERROR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Record a runtime error for retrieval by the AOT driver (no-op semantics
/// for the tiered JIT, which never inspects this slot).
pub fn aot_set_pending_error(msg: String) {
    AOT_PENDING_ERROR.with(|e| {
        *e.borrow_mut() = Some(msg);
    });
}

/// Take (and clear) the pending AOT runtime error, if any.
pub fn aot_take_pending_error() -> Option<String> {
    AOT_PENDING_ERROR.with(|e| e.borrow_mut().take())
}

pub unsafe fn set_jit_callbacks(cb: *mut dyn crate::vm::ActorVmCallbacks) {
    JIT_CALLBACKS.with(|cell| {
        *cell.get() = CbPair::from_ptr(cb);
    });
}

pub fn clear_jit_callbacks() {
    JIT_CALLBACKS.with(|cell| unsafe {
        *cell.get() = CbPair::NULL;
    });
}

// ---------------------------------------------------------------------------
// Constant-pool thread-local for JIT runtime helpers (string comparison)
// ---------------------------------------------------------------------------

/// Pointer-length pair for the current module's constant pool, stored as
/// two usize values to avoid zero-initialization UB in the thread-local.
#[derive(Clone, Copy)]
struct ConstantsPtr(*const Constant, usize);

impl ConstantsPtr {
    const NULL: Self = ConstantsPtr(std::ptr::null(), 0);

    /// # Safety
    /// The slice must be valid for the duration of the JIT execution.
    unsafe fn as_slice(self) -> &'static [Constant] {
        if self.0.is_null() {
            &[]
        } else {
            std::slice::from_raw_parts(self.0, self.1)
        }
    }
}

thread_local! {
    static JIT_CONSTANTS: UnsafeCell<ConstantsPtr> = UnsafeCell::new(ConstantsPtr::NULL);
}

/// Set the current module's constant pool for JIT runtime helpers.
///
/// # Safety
/// The slice must remain valid until `clear_jit_constants` is called.
pub unsafe fn set_jit_constants(constants: &[Constant]) {
    JIT_CONSTANTS.with(|cell| {
        *cell.get() = ConstantsPtr(constants.as_ptr(), constants.len());
    });
}

pub fn clear_jit_constants() {
    JIT_CONSTANTS.with(|cell| unsafe {
        *cell.get() = ConstantsPtr::NULL;
    });
}

/// Resolve a raw u64 value to its string content (for comparison).
/// Returns None for non-string values or when the constant pool is unavailable.
fn resolve_jit_string(raw: u64) -> Option<String> {
    if (raw & TAG_MASK) == TAG_STRING {
        // Interned string: look up in the thread-local constant pool.
        let id = (raw & PAYLOAD_MASK) as u32;
        JIT_CONSTANTS.with(|cell| unsafe {
            let cp = (*cell.get()).as_slice();
            match cp.get(id as usize) {
                Some(Constant::String(s)) => Some(s.clone()),
                _ => None,
            }
        })
    } else if (raw & TAG_MASK) == TAG_PTR {
        let ptr = (raw & PAYLOAD_MASK) as *mut u8;
        if ptr.is_null() {
            return None;
        }
        // SAFETY: ptr is a valid ActorHeap allocation with a header;
        // `heap_string_payload` re-checks the type tag and bounds the scan
        // for the NUL terminator by the recorded payload size.
        unsafe { heap_string_payload(ptr) }
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// JIT Safepoint: reduction-count preemption for long-running JIT regions
// ---------------------------------------------------------------------------

/// How many JIT region entries a behavior may execute before yielding
/// back to the scheduler. Reset at each behavior invocation.
pub const JIT_SAFEPOINT_BUDGET: u64 = 1000;

// JIT code can execute concurrently on different runtime worker threads. The
// status slots and safepoint target therefore must be thread-local: process-
// global slots let one VM consume another VM's branch exit or yield marker.
thread_local! {
    static JIT_SAFEPOINT_PTR: Cell<*mut u64> = const { Cell::new(std::ptr::null_mut()) };
    static JIT_YIELD_PC: Cell<u64> = const { Cell::new(u64::MAX) };
    static JIT_BRANCH_EXIT_PC: Cell<u64> = const { Cell::new(u64::MAX) };
}

pub fn set_jit_safepoint_ptr(ptr: *mut u64) {
    JIT_SAFEPOINT_PTR.with(|cell| cell.set(ptr));
}

pub fn clear_jit_safepoint_ptr() {
    JIT_SAFEPOINT_PTR.with(|cell| cell.set(std::ptr::null_mut()));
}

/// Check and decrement the current thread's actor reduction budget.
/// Returns 1 when the compiled region must yield, otherwise 0.
#[no_mangle]
pub extern "C" fn nulang_jit_safepoint_check(_unused: u64) -> u64 {
    JIT_SAFEPOINT_PTR.with(|cell| {
        let ptr = cell.get();
        if ptr.is_null() {
            return 0;
        }
        // SAFETY: the runtime installs a pointer to the active actor's
        // scheduler-confined counter for the duration of this JIT call.
        unsafe {
            let next = (*ptr).wrapping_sub(1);
            *ptr = next;
            u64::from((next as i64) <= 0)
        }
    })
}

/// Store a relative bytecode offset for a safepoint/effect fallback.
#[no_mangle]
pub extern "C" fn nulang_jit_set_yield_pc(offset: u64) -> u64 {
    JIT_YIELD_PC.with(|slot| slot.set(offset));
    0
}

/// Store a relative bytecode offset for a compiled branch exit.
#[no_mangle]
pub extern "C" fn nulang_jit_set_branch_exit_pc(offset: u64) -> u64 {
    JIT_BRANCH_EXIT_PC.with(|slot| slot.set(offset));
    0
}

/// Bytecode offset where the JIT yielded, or `u64::MAX` if no yield is
/// pending. Thread-local because multiple runtime workers can execute JIT
/// code concurrently.
pub fn take_jit_yield_pc() -> Option<usize> {
    JIT_YIELD_PC.with(|slot| {
        let old = slot.replace(u64::MAX);
        (old != u64::MAX).then_some(old as usize)
    })
}

/// Bytecode offset where a compiled region exited via a branch to a target
/// outside the region. Thread-local for the same reason as the yield slot.
pub fn take_jit_branch_exit_pc() -> Option<usize> {
    JIT_BRANCH_EXIT_PC.with(|slot| {
        let old = slot.replace(u64::MAX);
        (old != u64::MAX).then_some(old as usize)
    })
}

// ---------------------------------------------------------------------------
// Re-entrant direct-call support (JIT-compiled `Call` of a non-suspending
// callee). The compiled region stays resident in native code and invokes
// `nulang_jit_direct_call`, which runs the callee on the VM's interpreter
// frame stack to completion, then writes the result back into the region's
// register buffer. The callee is statically gated `!may_suspend`, so it
// never suspends mid-execution (which a re-run-from-the-call-start would
// mishandle). This is the correctness-critical seam between native regions
// and the interpreter.
// ---------------------------------------------------------------------------

// The single-threaded VM currently executing a compiled region (set by
// `VM::step` around each `tiered_execute_step_typed` call; the VM is
// thread-confined, so a thread-local pointer is sound).
thread_local! {
    static JIT_VM: Cell<*mut crate::vm::VM> = Cell::new(std::ptr::null_mut());
}

pub unsafe fn set_jit_vm(vm: *mut crate::vm::VM) {
    JIT_VM.with(|cell| cell.set(vm));
}

pub fn clear_jit_vm() {
    JIT_VM.with(|cell| cell.set(std::ptr::null_mut()));
}

fn get_jit_vm() -> *mut crate::vm::VM {
    JIT_VM.with(|cell| cell.get())
}

// Runtime error raised while running a re-entrant callee (e.g. step-limit).
// The compiled region cannot unwind, so the helper records it here and
// returns a nonzero status; the region jumps to its exit and `VM::step`
// checks this slot after the region returns.
thread_local! {
    static JIT_PENDING_VM_ERROR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

pub fn set_jit_pending_vm_error(msg: String) {
    JIT_PENDING_VM_ERROR.with(|e| *e.borrow_mut() = Some(msg));
}

pub fn take_jit_pending_vm_error() -> Option<String> {
    JIT_PENDING_VM_ERROR.with(|e| e.borrow_mut().take())
}

/// Snapshot of the JIT thread-local state a compiled region relies on, saved
/// around a re-entrant interpreter run and restored afterwards. Without this,
/// the nested execution would clobber the outer region's constant pool /
/// callbacks / safepoint / branch markers.
struct JitThreadState {
    vm: *mut crate::vm::VM,
    constants: ConstantsPtr,
    callbacks: CbPair,
    safepoint: *mut u64,
    yield_pc: u64,
    branch_exit_pc: u64,
    pending_error: Option<String>,
}

fn save_jit_thread_state() -> JitThreadState {
    let (vm, constants, callbacks) = (
        JIT_VM.with(|c| c.get()),
        JIT_CONSTANTS.with(|c| unsafe { *c.get() }),
        JIT_CALLBACKS.with(|c| unsafe { *c.get() }),
    );
    let (safepoint, yield_pc, branch_exit_pc) = (
        JIT_SAFEPOINT_PTR.with(|c| c.get()),
        JIT_YIELD_PC.with(|c| c.get()),
        JIT_BRANCH_EXIT_PC.with(|c| c.get()),
    );
    let pending_error = AOT_PENDING_ERROR.with(|e| e.borrow().clone());
    JitThreadState {
        vm,
        constants,
        callbacks,
        safepoint,
        yield_pc,
        branch_exit_pc,
        pending_error,
    }
}

fn restore_jit_thread_state(s: JitThreadState) {
    JIT_VM.with(|c| c.set(s.vm));
    unsafe {
        JIT_CONSTANTS.with(|c| *c.get() = s.constants);
        JIT_CALLBACKS.with(|c| *c.get() = s.callbacks);
    }
    JIT_SAFEPOINT_PTR.with(|c| c.set(s.safepoint));
    JIT_YIELD_PC.with(|c| c.set(s.yield_pc));
    JIT_BRANCH_EXIT_PC.with(|c| c.set(s.branch_exit_pc));
    AOT_PENDING_ERROR.with(|e| *e.borrow_mut() = s.pending_error);
}

/// Run a provably-non-suspending callee (function-table index `func_idx`) to
/// completion on the VM's interpreter frame stack, using the args already
/// staged in the region's `regs[0..argc]`, then write the callee's return
/// value into `regs[dst]`. Returns 0 on success, nonzero when the callee
/// raised (e.g. step-limit exceeded). The frame/step machinery lives in
/// `VM::jit_direct_call` (which has access to the VM's private frame state);
/// this wrapper only saves/restores the JIT thread-local state around the
/// re-entrant interpreter run so the outer compiled region's constant pool /
/// callbacks / safepoint / branch markers survive the nested execution.
///
/// # Safety
/// `regs` must point at the 256-entry register buffer of the compiled region
/// that invoked this helper, and `JIT_VM` must be set (a `VM` mid-`step()`).
/// The callee is `!may_suspend` by construction (the compiler gates emission
/// on that analysis), so it never suspends mid-run.
#[no_mangle]
pub extern "C" fn nulang_jit_direct_call(
    regs: *mut u64,
    func_idx: i64,
    argc: i64,
    dst: i64,
) -> i64 {
    let vm_ptr = get_jit_vm();
    if vm_ptr.is_null() || regs.is_null() {
        set_jit_pending_vm_error("JIT direct call with no active VM".to_string());
        return 1;
    }
    let vm = unsafe { &mut *vm_ptr };
    let saved = save_jit_thread_state();
    let status = vm.jit_direct_call(regs, func_idx as usize, argc as usize, dst as usize);
    restore_jit_thread_state(saved);
    status
}

/// Called from JIT-compiled code when the safepoint budget is exhausted.
#[no_mangle]
pub unsafe extern "C" fn nulang_safepoint_yield(resume_offset: u64) -> u64 {
    nulang_jit_set_yield_pc(resume_offset)
}

unsafe fn with_callbacks<R>(f: impl FnOnce(&mut dyn crate::vm::ActorVmCallbacks) -> R) -> R {
    JIT_CALLBACKS.with(|cell| {
        let pair = *cell.get();
        assert!(!pair.is_null(), "JIT_CALLBACKS not set");
        f(&mut *pair.to_ptr())
    })
}

use crate::runtime::heap::{ActorHeap, TypeTag as HeapTypeTag};

// ---------------------------------------------------------------------------
// AOT standalone execution context
// ---------------------------------------------------------------------------

thread_local! {
    /// Standalone heap for AOT execution when no actor runtime is active.
    static AOT_HEAP: std::cell::RefCell<Option<crate::runtime::heap::ActorHeap>> =
        std::cell::RefCell::new(None);
    /// Standalone constant pool for AOT execution.
    static AOT_CONSTANTS: std::cell::RefCell<Option<Vec<crate::bytecode::Constant>>> =
        std::cell::RefCell::new(None);
}

/// Set up a standalone heap for AOT execution.
pub fn aot_set_heap(heap: crate::runtime::heap::ActorHeap) {
    AOT_HEAP.with(|cell| {
        *cell.borrow_mut() = Some(heap);
    });
}

/// Take the standalone heap, returning it to the caller.
pub fn aot_take_heap() -> Option<crate::runtime::heap::ActorHeap> {
    AOT_HEAP.with(|cell| cell.borrow_mut().take())
}

/// Set standalone constants for AOT execution.
///
/// # Safety
/// The slice must remain valid until `aot_clear_constants` is called.
pub unsafe fn aot_set_constants(constants: &[crate::bytecode::Constant]) {
    AOT_CONSTANTS.with(|cell| {
        *cell.borrow_mut() = Some(constants.to_vec());
    });
}

/// Clear standalone constants.
pub fn aot_clear_constants() {
    AOT_CONSTANTS.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Allocate via callbacks or fall back to standalone AOT heap.
/// Check if JIT callbacks are set, and if so, use them.
pub(crate) unsafe fn try_with_callbacks<R>(
    f: impl FnOnce(&mut dyn crate::vm::ActorVmCallbacks) -> R,
) -> Option<R> {
    JIT_CALLBACKS.with(|cell| {
        let pair = *cell.get();
        if pair.is_null() {
            None
        } else {
            Some(f(&mut *pair.to_ptr()))
        }
    })
}

/// Allocate via callbacks or fall back to standalone AOT heap.
unsafe fn alloc_obj(size: usize, type_tag: HeapTypeTag) -> Option<*mut u8> {
    if let Some(ptr) = try_with_callbacks(|cb| cb.alloc(size, type_tag)) {
        return ptr;
    }
    AOT_HEAP.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .and_then(|heap| heap.alloc(size, type_tag))
    })
}

/// Retain a reference via callbacks or AOT heap directly.
unsafe fn retain_obj(ptr: *mut u8) {
    if try_with_callbacks(|cb| {
        cb.retain_ref(ptr);
        true
    })
    .is_some()
    {
        return;
    }
    if !ptr.is_null() {
        let header = &mut *ActorHeap::header_of(ptr);
        header.ref_count += 1;
    }
}

/// Drop a reference via callbacks or AOT heap directly.
unsafe fn drop_obj(ptr: *mut u8) {
    if try_with_callbacks(|cb| {
        cb.drop_ref(ptr);
        true
    })
    .is_some()
    {
        return;
    }
    if !ptr.is_null() {
        let header = &mut *ActorHeap::header_of(ptr);
        if header.ref_count > 0 {
            header.ref_count -= 1;
        }
        if header.ref_count == 0 {
            AOT_HEAP.with(|cell| {
                if let Some(ref mut heap) = *cell.borrow_mut() {
                    heap.free(ptr);
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// AOT value-based runtime helpers
// ---------------------------------------------------------------------------

/// Allocate a heap object with `slot_count` slots of type `type_tag`.
/// Returns tagged pointer or nil.
#[no_mangle]
pub unsafe extern "C" fn nulang_alloc_obj(slot_count: u64, type_tag_raw: u32) -> u64 {
    let count = slot_count as usize;
    let tag: HeapTypeTag = match type_tag_raw {
        1 => HeapTypeTag::Array,
        3 => HeapTypeTag::Record,
        6 => HeapTypeTag::Tuple,
        2 => HeapTypeTag::String,
        _ => return Value::nil().as_raw(),
    };
    let size = count.checked_mul(std::mem::size_of::<Value>()).unwrap_or(0);
    if let Some(ptr) = alloc_obj(size, tag) {
        let slots = std::slice::from_raw_parts_mut(ptr as *mut Value, count);
        for slot in slots.iter_mut() {
            *slot = Value::nil();
        }
        Value::ptr(ptr).as_raw()
    } else {
        Value::nil().as_raw()
    }
}

/// Read slot `idx` from a heap object (record, tuple, or array).
/// Returns nil if the object is not a valid heap object or idx is out of range.
#[no_mangle]
pub unsafe extern "C" fn nulang_obj_get(obj: u64, idx: u64) -> u64 {
    let obj_ptr = val_ptr(obj);
    if obj_ptr.is_null() {
        return Value::nil().as_raw();
    }
    let header = &*ActorHeap::header_of(obj_ptr);
    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
    let len = payload_size / std::mem::size_of::<Value>();
    // idx may be a raw slot index (records/tuples, unboxed arrays) or a tagged
    // Int (boxed arrays) — mask off any tag bits to get the slot position.
    let i = (idx & PAYLOAD_MASK) as usize;
    if i < len {
        (*((obj_ptr as *const Value).add(i))).as_raw()
    } else {
        Value::nil().as_raw()
    }
}

/// Write `val` into slot `idx` of a heap object, with proper refcounting.
#[no_mangle]
pub unsafe extern "C" fn nulang_obj_set(obj: u64, idx: u64, val: u64) {
    let obj_ptr = val_ptr(obj);
    if obj_ptr.is_null() {
        return;
    }
    let val = Value::from_raw(val);
    let header = &*ActorHeap::header_of(obj_ptr);
    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
    let len = payload_size / std::mem::size_of::<Value>();
    // idx may be a raw slot index (records/tuples, unboxed arrays) or a tagged
    // Int (boxed arrays) — mask off any tag bits to get the slot position.
    let i = (idx & PAYLOAD_MASK) as usize;
    if i < len {
        if let Some(ptr) = val.as_ptr() {
            retain_obj(ptr);
        }
        let slot = (obj_ptr as *mut Value).add(i);
        let old = *slot;
        *slot = val;
        if let Some(old_ptr) = old.as_ptr() {
            drop_obj(old_ptr);
        }
    }
}

/// Get element count of a heap object (record, tuple, or array).
/// Returns tagged int.
#[no_mangle]
pub unsafe extern "C" fn nulang_obj_len(obj: u64) -> u64 {
    let obj_ptr = val_ptr(obj);
    if obj_ptr.is_null() {
        return Value::int(0).as_raw();
    }
    let header = &*ActorHeap::header_of(obj_ptr);
    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
    let len = payload_size / std::mem::size_of::<Value>();
    Value::int(len as i64).as_raw()
}

/// Shallow copy a record (copies all slots, retains each).
/// Returns tagged pointer or nil.
#[no_mangle]
pub unsafe extern "C" fn nulang_rec_copy(obj: u64) -> u64 {
    let src_ptr = val_ptr(obj);
    if src_ptr.is_null() {
        return Value::nil().as_raw();
    }
    let header = &*ActorHeap::header_of(src_ptr);
    if header.type_tag != HeapTypeTag::Record {
        return Value::nil().as_raw();
    }
    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
    let slot_count = payload_size / std::mem::size_of::<Value>();
    if let Some(dst_ptr) = alloc_obj(payload_size, HeapTypeTag::Record) {
        let src_slots = std::slice::from_raw_parts(src_ptr as *const Value, slot_count);
        let dst_slots = std::slice::from_raw_parts_mut(dst_ptr as *mut Value, slot_count);
        for i in 0..slot_count {
            let val = src_slots[i];
            if let Some(ptr) = val.as_ptr() {
                retain_obj(ptr);
            }
            dst_slots[i] = val;
        }
        Value::ptr(dst_ptr).as_raw()
    } else {
        Value::nil().as_raw()
    }
}

/// String equality: compare two Nulang values as strings.
/// Returns tagged bool.
#[no_mangle]
pub unsafe extern "C" fn nulang_str_eq(a: u64, b: u64) -> u64 {
    let sa = resolve_string_coerce(a);
    let sb = resolve_string_coerce(b);
    let eq = match (sa, sb) {
        (Some(sa), Some(sb)) => sa == sb,
        _ => false,
    };
    Value::bool(eq).as_raw()
}

/// String concatenation: allocate a new heap string.
/// Returns tagged pointer or nil.
#[no_mangle]
pub fn resolve_string_coerce(raw: u64) -> Option<String> {
    let val = crate::vm::Value::from_raw(raw);
    if val.is_int() {
        return Some(val.as_int().unwrap().to_string());
    }
    if val.is_float() {
        return Some(val.as_float().unwrap().to_string());
    }
    if val.is_bool() {
        return Some(val.as_bool().unwrap().to_string());
    }
    if (raw & TAG_MASK) == TAG_STRING {
        // String constant from the module pool: content lives in the JIT or
        // AOT constant pool, keyed by the payload index.
        let id = (raw & PAYLOAD_MASK) as u32;
        let from_jit = JIT_CONSTANTS.with(|cell| unsafe {
            let cp = (*cell.get()).as_slice();
            cp.get(id as usize).and_then(|c| match c {
                crate::bytecode::Constant::String(s) => Some(s.clone()),
                _ => None,
            })
        });
        if from_jit.is_some() {
            return from_jit;
        }
        return AOT_CONSTANTS.with(|cell| {
            let guard = cell.borrow();
            if let Some(ref constants) = *guard {
                constants.get(id as usize).and_then(|c| match c {
                    crate::bytecode::Constant::String(s) => Some(s.clone()),
                    _ => None,
                })
            } else {
                None
            }
        });
    }
    if (raw & TAG_MASK) == TAG_PTR {
        let ptr = (raw & PAYLOAD_MASK) as *mut u8;
        if ptr.is_null() {
            return None;
        }
        // SAFETY: TAG_PTR values reaching the JIT runtime are produced by
        // `alloc_obj` on this thread's actor heap; `heap_string_payload`
        // re-checks the type tag and bounds the NUL scan.
        unsafe {
            return heap_string_payload(ptr);
        }
    }
    None
}

/// Read the string content of a heap string payload, or `None` when `ptr`
/// does not point at a `HeapTypeTag::String` allocation.
///
/// Unlike a bare `CStr::from_ptr`, the scan for the NUL terminator is
/// bounded by the payload size recorded in the object header, so a missing
/// terminator (heap corruption or a foreign-constructed value) cannot read
/// past the allocation.
///
/// # Safety
/// `ptr` must be a live payload pointer returned by `alloc_obj` (or the
/// equivalent actor-heap allocation) on this thread.
unsafe fn heap_string_payload(ptr: *mut u8) -> Option<String> {
    let header = &*ActorHeap::header_of(ptr);
    if header.type_tag != HeapTypeTag::String {
        return None;
    }
    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
    // SAFETY: `ptr..ptr+payload_size` is inside this live allocation.
    let bytes = std::slice::from_raw_parts(ptr, payload_size);
    // Find the NUL terminator within the allocation; fall back to the full
    // payload (matching `to_string_lossy` behavior for unterminated data)
    // instead of reading out of bounds.
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

#[no_mangle]
pub unsafe extern "C" fn nulang_str_concat(a: u64, b: u64) -> u64 {
    let result = format!("{}{}", coerce_string(a), coerce_string(b));
    alloc_string_value(result)
}

/// Power operation: float pow when both operands are floats; int pow using
/// binary exponentiation with `wrapping_mul` when both are ints — bit-for-bit
/// the interpreter's `step_ipow`, so the 48-bit payload wraps on overflow
/// (matching `IMul`/`IAdd` behaviour) rather than erroring. A negative int
/// exponent returns nil (mirrors `IDiv` div-by-zero); 0 ** 0 returns 1
/// (standard convention). Non-numeric operands coerce to 0, exactly like the
/// interpreter's `as_int().unwrap_or(0)`.
#[no_mangle]
pub extern "C" fn nulang_pow(a: u64, b: u64) -> u64 {
    if is_float_raw(a) && is_float_raw(b) {
        let af = f64::from_bits(a);
        let bf = f64::from_bits(b);
        return Value::float(af.powf(bf)).as_raw();
    }
    let va = Value::from_raw(a);
    let vb = Value::from_raw(b);
    let base = va.as_int().unwrap_or(0);
    let exp = vb.as_int().unwrap_or(0);
    if exp < 0 {
        return Value::nil().as_raw();
    }
    // Binary exponentiation with wrapping_mul — mirrors `step_ipow` exactly
    // so overflow wraps (truncated to the 48-bit payload by `Value::int`)
    // instead of recording an arithmetic error.
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
    Value::int(result).as_raw()
}

/// # Safety
/// `regs` must point to a valid `[u64; 256]` array. Called only from
/// JIT-compiled code that follows the `regs_ptr` ABI contract.
#[no_mangle]
pub unsafe extern "C" fn nulang_arr_store(
    regs: *mut u64,
    arr_reg: u32,
    idx_reg: u32,
    src_reg: u32,
) {
    let arr_ptr_val = *regs.add(arr_reg as usize);
    let idx_val = *regs.add(idx_reg as usize);
    let val = Value::from_raw(*regs.add(src_reg as usize));
    let arr_ptr = val_ptr(arr_ptr_val);
    if arr_ptr.is_null() {
        return;
    }
    let idx = as_int_or_zero(idx_val) as usize;
    with_callbacks(|cb| {
        if let Some(len) = cb.array_len(arr_ptr) {
            if idx < len {
                if let Some(ptr) = val.as_ptr() {
                    cb.retain_ref(ptr);
                }
                let slot = (arr_ptr as *mut Value).add(idx);
                let old = *slot;
                *slot = val;
                if let Some(old_ptr) = old.as_ptr() {
                    cb.drop_ref(old_ptr);
                }
            }
        }
    });
}

/// # Safety
/// `regs` must point to a valid `[u64; 256]` array.
#[no_mangle]
pub unsafe extern "C" fn nulang_arr_len(regs: *mut u64, arr_reg: u32, dst_reg: u32) {
    let arr_ptr_val = *regs.add(arr_reg as usize);
    let arr_ptr = val_ptr(arr_ptr_val);
    let len = if !arr_ptr.is_null() {
        let header = &*ActorHeap::header_of(arr_ptr);
        if header.type_tag == HeapTypeTag::Array {
            header.size.saturating_sub(ActorHeap::HEADER_SIZE) / std::mem::size_of::<Value>()
        } else {
            0
        }
    } else {
        0
    };
    *regs.add(dst_reg as usize) = tag_int(len as i64);
}

/// # Safety
/// `regs` must point to a valid `[u64; 256]` array.
#[no_mangle]
pub unsafe extern "C" fn nulang_field_load(regs: *mut u64, obj_reg: u32, idx: u32, dst_reg: u32) {
    let obj_ptr_val = *regs.add(obj_reg as usize);
    let obj_ptr = val_ptr(obj_ptr_val);
    let val = if !obj_ptr.is_null() {
        let header = &*ActorHeap::header_of(obj_ptr);
        if header.type_tag == HeapTypeTag::Tuple {
            let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
            let len = payload_size / std::mem::size_of::<Value>();
            if (idx as usize) < len {
                *((obj_ptr as *const Value).add(idx as usize))
            } else {
                Value::nil()
            }
        } else {
            Value::nil()
        }
    } else {
        Value::nil()
    };
    *regs.add(dst_reg as usize) = val.as_raw();
}

// ---------------------------------------------------------------------------
// AOT actor runtime helpers
// ---------------------------------------------------------------------------
// Called from AOT-compiled code when the function body contains actor
// operations (SelfRef, StateGet, StateSet).  They go through the same
// `ActorVmCallbacks` trait the VM uses, stored in the JIT_CALLBACKS
// thread-local (set before each AOT invocation).  Outside an actor
// context (`try_with_callbacks` returns None) they degrade gracefully:
// SelfRef/StateGet return nil, StateSet is a no-op.

/// Return the current actor's ID as a tagged i64, or nil outside an actor.
#[no_mangle]
pub unsafe extern "C" fn nulang_aot_self_ref() -> u64 {
    try_with_callbacks(|cb| match cb.current_actor_id() {
        Some(id) => Value::int(id as i64).as_raw(),
        None => Value::nil().as_raw(),
    })
    .unwrap_or_else(|| Value::nil().as_raw())
}

/// Read a field from the current actor's durable state.
///
/// `field_name_raw` is a TAG_STRING constant resolved via
/// `resolve_string_coerce`. Returns nil when no actor is active or the
/// field is absent.
#[no_mangle]
pub unsafe extern "C" fn nulang_aot_state_get(field_name_raw: u64) -> u64 {
    let field = resolve_string_coerce(field_name_raw).unwrap_or_default();
    try_with_callbacks(|cb| cb.get_state_field(&field).as_raw())
        .unwrap_or_else(|| Value::nil().as_raw())
}

/// Write a field on the current actor's durable state.
///
/// `field_name_raw` is a TAG_STRING constant; `value` is the new
/// NaN-tagged value to store. No-op outside an actor.
#[no_mangle]
pub unsafe extern "C" fn nulang_aot_state_set(field_name_raw: u64, value: u64) {
    let field = resolve_string_coerce(field_name_raw).unwrap_or_default();
    try_with_callbacks(|cb| cb.set_state_field(&field, Value::from_bits(value)));
}

// ---------------------------------------------------------------------------
// AOT fire-and-forget message send
// ---------------------------------------------------------------------------
// `send actor behavior(args...)` in an AOT-compiled behavior lowers to a call
// to one of these arity-matched helpers (0..8 payload args). The helper packs
// the boxed args and routes through the current callbacks' `send_message`,
// which delivers to the target actor's mailbox (scheduler path) or a
// registered standalone actor (AOT dispatch path). Outside an actor context
// it is a no-op, matching the bytecode VM's outside-an-actor contract.

macro_rules! define_aot_send {
    ($name:ident, $($arg:ident),*) => {
        /// Send a fire-and-forget actor message from AOT-compiled code.
        #[no_mangle]
        pub unsafe extern "C" fn $name(target_raw: u64, behavior_raw: u64 $(, $arg: u64)*) {
            let args = [$(Value::from_bits($arg)),*];
            let _ = try_with_callbacks(|cb| {
                cb.send_message(Value::from_bits(target_raw), behavior_raw as u16, &args);
                true
            });
        }
    };
}

define_aot_send!(nulang_aot_send_0,);
define_aot_send!(nulang_aot_send_1, a0);
define_aot_send!(nulang_aot_send_2, a0, a1);
define_aot_send!(nulang_aot_send_3, a0, a1, a2);
define_aot_send!(nulang_aot_send_4, a0, a1, a2, a3);
define_aot_send!(nulang_aot_send_5, a0, a1, a2, a3, a4);
define_aot_send!(nulang_aot_send_6, a0, a1, a2, a3, a4, a5);
define_aot_send!(nulang_aot_send_7, a0, a1, a2, a3, a4, a5, a6);
define_aot_send!(nulang_aot_send_8, a0, a1, a2, a3, a4, a5, a6, a7);

// ---------------------------------------------------------------------------
// AOT event emission
// ---------------------------------------------------------------------------
// `emit Event(args)` in an AOT-compiled behavior lowers to an arity-matched
// `nulang_aot_emit_N` call. The helper resolves the event name (a TAG_STRING
// constant from the module pool), packs the boxed args, and routes through the
// current callbacks' `emit_event`, which records the event on the target actor
// (`actor.event_log`) exactly as the bytecode `Emit` opcode does. Outside an
// actor context it is a no-op.

macro_rules! define_aot_emit {
    ($name:ident, $($arg:ident),*) => {
        /// Emit an event from AOT-compiled code.
        #[no_mangle]
        pub unsafe extern "C" fn $name(event_raw: u64 $(, $arg: u64)*) {
            let event = resolve_string_coerce(event_raw).unwrap_or_default();
            let args = [$(Value::from_bits($arg)),*];
            let _ = try_with_callbacks(|cb| {
                cb.emit_event(&event, &args);
                true
            });
        }
    };
}

define_aot_emit!(nulang_aot_emit_0,);
define_aot_emit!(nulang_aot_emit_1, a0);
define_aot_emit!(nulang_aot_emit_2, a0, a1);
define_aot_emit!(nulang_aot_emit_3, a0, a1, a2);
define_aot_emit!(nulang_aot_emit_4, a0, a1, a2, a3);
define_aot_emit!(nulang_aot_emit_5, a0, a1, a2, a3, a4);
define_aot_emit!(nulang_aot_emit_6, a0, a1, a2, a3, a4, a5);
define_aot_emit!(nulang_aot_emit_7, a0, a1, a2, a3, a4, a5, a6);
define_aot_emit!(nulang_aot_emit_8, a0, a1, a2, a3, a4, a5, a6, a7);

// ---------------------------------------------------------------------------
// AOT synchronous ask
// ---------------------------------------------------------------------------
// `ask actor behavior(args)` in AOT-compiled code lowers to an arity-matched
// `nulang_aot_ask_N` call. The helper packs the boxed args and routes through
// the current callbacks' `ask_actor`, which performs a synchronous
// request-response (`Runtime::ask_actor_sync` under the actor runtime, the
// same path the bytecode `Ask` opcode takes). Outside an actor context it
// degrades to nil, matching the standalone VM's default.

macro_rules! define_aot_ask {
    ($name:ident, $($arg:ident),*) => {
        /// Perform a synchronous ask (request-response) from AOT-compiled code.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            actor_raw: u64,
            behavior_raw: u64 $(, $arg: u64)*,
        ) -> u64 {
            let args = [$(Value::from_bits($arg)),*];
            try_with_callbacks(|cb| {
                cb.ask_actor(
                    Value::from_bits(actor_raw),
                    behavior_raw as u16,
                    &args,
                )
                .as_raw()
            })
            .unwrap_or(Value::nil().as_raw())
        }
    };
}

define_aot_ask!(nulang_aot_ask_0,);
define_aot_ask!(nulang_aot_ask_1, a0);
define_aot_ask!(nulang_aot_ask_2, a0, a1);
define_aot_ask!(nulang_aot_ask_3, a0, a1, a2);
define_aot_ask!(nulang_aot_ask_4, a0, a1, a2, a3);
define_aot_ask!(nulang_aot_ask_5, a0, a1, a2, a3, a4);
define_aot_ask!(nulang_aot_ask_6, a0, a1, a2, a3, a4, a5);
define_aot_ask!(nulang_aot_ask_7, a0, a1, a2, a3, a4, a5, a6);
define_aot_ask!(nulang_aot_ask_8, a0, a1, a2, a3, a4, a5, a6, a7);

// ---------------------------------------------------------------------------
// AOT foreign function calls
// ---------------------------------------------------------------------------
// `extern "..." { fn sym(args) -> ret }` invoked from AOT-compiled code lowers
// to an arity-matched `nulang_aot_ffi_call_N` call. The helper receives the
// library and symbol names as TAG_STRING pool constants, a bit-packed
// signature (low 3 bits = return CType tag, then 3 bits per parameter), and
// the boxed argument values. It resolves the native function through the
// global FFI registry (the same `resolve_or_load` + `call_native` path the
// bytecode FFICall opcode uses), marshals CStr parameters into temporary
// CStrings, and invokes it. CStr returns are rejected at compile time (the
// AOT backend has no module pool to intern the result string into) and the
// sandbox allow-list is not applied because AOT native code is trusted.

pub(crate) fn aot_ctype_from_tag(tag: u64) -> crate::ffi::marshal::CType {
    match tag {
        0 => crate::ffi::marshal::CType::I64,
        1 => crate::ffi::marshal::CType::F64,
        2 => crate::ffi::marshal::CType::Bool,
        3 => crate::ffi::marshal::CType::CStr,
        4 => crate::ffi::marshal::CType::VoidPtr,
        _ => crate::ffi::marshal::CType::Unit,
    }
}

fn aot_ffi_call_impl(lib_raw: u64, sym_raw: u64, sig: u64, args: &[u64]) -> Value {
    let library = resolve_string_coerce(lib_raw).unwrap_or_default();
    let symbol = resolve_string_coerce(sym_raw).unwrap_or_default();
    let ret_tag = sig & 0b111;
    let mut params: Vec<crate::ffi::marshal::CType> = Vec::with_capacity(args.len());
    for i in 0..args.len() {
        let tag = (sig >> (3 + 3 * i as u32)) & 0b111;
        params.push(aot_ctype_from_tag(tag));
    }
    let ret = aot_ctype_from_tag(ret_tag);
    let signature = crate::ffi::marshal::Signature::new(params.clone(), ret);
    let func = {
        let registry = crate::ffi::native::FFI_REGISTRY
            .get_or_init(|| std::sync::Mutex::new(crate::ffi::native::FfiRegistry::new()));
        let mut reg = match registry.lock() {
            Ok(r) => r,
            Err(_) => return Value::nil(),
        };
        // SAFETY: the signature encodes the declared extern type; resolution
        // finds a pre-registered function or loads the shared library.
        match unsafe { reg.resolve_or_load(&library, &symbol, signature) } {
            Ok(f) => f,
            Err(_) => return Value::nil(),
        }
    };
    // Marshal CStr parameters: copy Nulang string values into temporary
    // CStrings whose pointers remain valid for the duration of the call.
    let mut cstrings: Vec<std::ffi::CString> = Vec::new();
    let mut cargs: Vec<Value> = Vec::with_capacity(args.len());
    for (i, p) in params.iter().enumerate() {
        if *p == crate::ffi::marshal::CType::CStr {
            let bytes = resolve_string_coerce(args[i]).unwrap_or_default();
            let c = match std::ffi::CString::new(bytes) {
                Ok(c) => c,
                Err(_) => return Value::nil(),
            };
            cargs.push(Value::ptr(c.as_ptr() as *mut u8));
            cstrings.push(c);
        } else {
            cargs.push(Value::from_bits(args[i]));
        }
    }
    // SAFETY: func.ptr points to a function whose ABI matches the signature.
    match unsafe { crate::ffi::marshal::call_native(&func, &cargs) } {
        Ok(v) => v,
        Err(_) => Value::nil(),
    }
}

macro_rules! define_aot_ffi_call {
    ($name:ident, $($arg:ident),*) => {
        /// Invoke a foreign function from AOT-compiled code.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            lib_raw: u64,
            sym_raw: u64,
            sig: u64 $(, $arg: u64)*,
        ) -> u64 {
            let args = [$($arg),*];
            aot_ffi_call_impl(lib_raw, sym_raw, sig, &args).as_raw()
        }
    };
}

define_aot_ffi_call!(nulang_aot_ffi_call_0,);
define_aot_ffi_call!(nulang_aot_ffi_call_1, a0);
define_aot_ffi_call!(nulang_aot_ffi_call_2, a0, a1);
define_aot_ffi_call!(nulang_aot_ffi_call_3, a0, a1, a2);
define_aot_ffi_call!(nulang_aot_ffi_call_4, a0, a1, a2, a3);

// ---------------------------------------------------------------------------
// AOT closures with captures
// ---------------------------------------------------------------------------
// A captured closure in AOT-compiled code is a `TAG_CLOSURE` value pointing at
// a heap object `[fn_idx, cap_count, cap0, ..., capN-1]` allocated from the
// actor heap (TypeTag::Closure). Creation lowers to an arity-matched
// `nulang_aot_make_closure_N`; calling a captured closure lowers to an
// arity-matched `nulang_aot_call_closure_N`, which reads the function index
// and captured values back out, resolves the compiled function pointer through
// the armed AOT module (`nulang_aot_resolve_fn`), and invokes it with
// (explicit args + captures) — matching the lifted function's
// param-then-capture signature.

macro_rules! define_aot_make_closure {
    ($name:ident, $($cap:ident),*) => {
        /// Allocate a closure object carrying fn_idx and the captured values.
        #[no_mangle]
        pub unsafe extern "C" fn $name(fn_idx: u64 $(, $cap: u64)*) -> u64 {
            let count: usize = 0 $(+ { let _ = stringify!($cap); 1 })*;
            let Some(ptr) = alloc_obj(2 + count * 8, HeapTypeTag::Closure) else {
                return Value::nil().as_raw();
            };
            let slot = ptr as *mut u64;
            *slot = fn_idx;
            *slot.add(1) = count as u64;
            let caps = [$($cap),*];
            for (i, c) in caps.iter().enumerate() {
                *slot.add(2 + i) = *c;
            }
            (TAG_CLOSURE | ptr as u64)
        }
    };
}

define_aot_make_closure!(nulang_aot_make_closure_0,);
define_aot_make_closure!(nulang_aot_make_closure_1, a0);
define_aot_make_closure!(nulang_aot_make_closure_2, a0, a1);
define_aot_make_closure!(nulang_aot_make_closure_3, a0, a1, a2);
define_aot_make_closure!(nulang_aot_make_closure_4, a0, a1, a2, a3);
define_aot_make_closure!(nulang_aot_make_closure_5, a0, a1, a2, a3, a4);
define_aot_make_closure!(nulang_aot_make_closure_6, a0, a1, a2, a3, a4, a5);
define_aot_make_closure!(nulang_aot_make_closure_7, a0, a1, a2, a3, a4, a5, a6);
define_aot_make_closure!(nulang_aot_make_closure_8, a0, a1, a2, a3, a4, a5, a6, a7);

/// Invoke a compiled function pointer with a flat boxed-arg list (explicit
/// args followed by captured values), dispatching on the total arity.
unsafe fn call_closure_dispatch(fn_ptr: u64, all: &[u64]) -> u64 {
    match all.len() {
        0 => {
            let f: unsafe extern "C" fn() -> u64 = std::mem::transmute(fn_ptr);
            f()
        }
        1 => {
            let f: unsafe extern "C" fn(u64) -> u64 = std::mem::transmute(fn_ptr);
            f(all[0])
        }
        2 => {
            let f: unsafe extern "C" fn(u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(all[0], all[1])
        }
        3 => {
            let f: unsafe extern "C" fn(u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(all[0], all[1], all[2])
        }
        4 => {
            let f: unsafe extern "C" fn(u64, u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(all[0], all[1], all[2], all[3])
        }
        5 => {
            let f: unsafe extern "C" fn(u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(all[0], all[1], all[2], all[3], all[4])
        }
        6 => {
            let f: unsafe extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(all[0], all[1], all[2], all[3], all[4], all[5])
        }
        7 => {
            let f: unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(all[0], all[1], all[2], all[3], all[4], all[5], all[6])
        }
        8 => {
            let f: unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(
                all[0], all[1], all[2], all[3], all[4], all[5], all[6], all[7],
            )
        }
        9 => {
            let f: unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(
                all[0], all[1], all[2], all[3], all[4], all[5], all[6], all[7], all[8],
            )
        }
        10 => {
            let f: unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(
                all[0], all[1], all[2], all[3], all[4], all[5], all[6], all[7], all[8], all[9],
            )
        }
        11 => {
            let f: unsafe extern "C" fn(
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
            ) -> u64 = std::mem::transmute(fn_ptr);
            f(
                all[0], all[1], all[2], all[3], all[4], all[5], all[6], all[7], all[8], all[9],
                all[10],
            )
        }
        12 => {
            let f: unsafe extern "C" fn(
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
                u64,
            ) -> u64 = std::mem::transmute(fn_ptr);
            f(
                all[0], all[1], all[2], all[3], all[4], all[5], all[6], all[7], all[8], all[9],
                all[10], all[11],
            )
        }
        _ => Value::nil().as_raw(),
    }
}

macro_rules! define_aot_call_closure {
    ($name:ident, $($arg:ident),*) => {
        /// Invoke a closure value: an uncaptured closure is a tagged fn index
        /// (dispatch with the explicit args only); a captured closure is a
        /// TAG_CLOSURE object carrying fn_idx + captures (dispatch with
        /// args + captures). Handles closures whose target is not statically
        /// known at the call site (e.g. passed as a parameter).
        #[no_mangle]
        pub unsafe extern "C" fn $name(closure_raw: u64 $(, $arg: u64)*) -> u64 {
            let args = [$($arg),*];
            let fn_ptr;
            let mut all: Vec<u64>;
            if (closure_raw & TAG_MASK) == TAG_INT {
                // Uncaptured closure: the tagged payload is the fn index.
                let fn_idx = (closure_raw & PAYLOAD_MASK) as i64;
                fn_ptr = crate::aot::nulang_aot_resolve_fn(fn_idx as u64);
                all = Vec::with_capacity(args.len());
                all.extend_from_slice(&args);
            } else if (closure_raw & TAG_MASK) == TAG_CLOSURE {
                // Captured closure object: [fn_idx, cap_count, cap0..].
                let ptr = (closure_raw & PAYLOAD_MASK) as *mut u64;
                if ptr.is_null() {
                    return Value::nil().as_raw();
                }
                let fn_idx = *ptr;
                let cap_count = *ptr.add(1) as usize;
                fn_ptr = crate::aot::nulang_aot_resolve_fn(fn_idx);
                all = Vec::with_capacity(args.len() + cap_count);
                all.extend_from_slice(&args);
                for i in 0..cap_count {
                    all.push(*ptr.add(2 + i));
                }
            } else {
                return Value::nil().as_raw();
            }
            if fn_ptr == 0 {
                return Value::nil().as_raw();
            }
            call_closure_dispatch(fn_ptr, &all)
        }
    };
}

define_aot_call_closure!(nulang_aot_call_closure_0,);
define_aot_call_closure!(nulang_aot_call_closure_1, a0);
define_aot_call_closure!(nulang_aot_call_closure_2, a0, a1);
define_aot_call_closure!(nulang_aot_call_closure_3, a0, a1, a2);
define_aot_call_closure!(nulang_aot_call_closure_4, a0, a1, a2, a3);
define_aot_call_closure!(nulang_aot_call_closure_5, a0, a1, a2, a3, a4);
define_aot_call_closure!(nulang_aot_call_closure_6, a0, a1, a2, a3, a4, a5);
define_aot_call_closure!(nulang_aot_call_closure_7, a0, a1, a2, a3, a4, a5, a6);
define_aot_call_closure!(nulang_aot_call_closure_8, a0, a1, a2, a3, a4, a5, a6, a7);

// ---------------------------------------------------------------------------
// AOT async effect dispatch
// ---------------------------------------------------------------------------
// `perform Effect.op(args)` for the async-effect family (LLM/Inference.ask,
// Timer.sleep, Pipeline.*, Supervisor.*) lowers to an arity-matched
// `nulang_aot_perform_async_N` call. The helper resolves the fully-qualified
// effect name from the module pool, routes through the current callbacks'
// `perform_async` (the same path the bytecode PerformAsync opcode takes), and
// materializes the result: Ready(Some(content)) becomes a heap string,
// Ready(None) and unarmed callbacks become nil. Effects that return Pending
// (LLM.ask, Timer.sleep with a positive delay) degrade to nil — the native
// backend has no VM suspension, so the actor cannot be parked mid-behavior.

macro_rules! define_aot_perform_async {
    ($name:ident, $($arg:ident),*) => {
        /// Dispatch an async effect from AOT-compiled code.
        #[no_mangle]
        pub unsafe extern "C" fn $name(effect_raw: u64 $(, $arg: u64)*) -> u64 {
            let args = [$($arg),*];
            let effect_op = resolve_string_coerce(effect_raw).unwrap_or_default();
            let constants = crate::aot::aot_module_constants();
            let vals: Vec<Value> = args.iter().map(|a| Value::from_bits(*a)).collect();
            match try_with_callbacks(|cb| cb.perform_async(&effect_op, constants, &vals)) {
                Some(crate::vm::PerformAsyncResult::Ready(Some(content))) => {
                    let bytes = content.into_bytes();
                    if let Some(ptr) = alloc_obj(bytes.len() + 1, HeapTypeTag::String) {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                        Value::ptr(ptr).as_raw()
                    } else {
                        Value::nil().as_raw()
                    }
                }
                // Ready(None), unarmed callbacks, or Pending (no native
                // suspension) all degrade to nil.
                _ => Value::nil().as_raw(),
            }
        }
    };
}

define_aot_perform_async!(nulang_aot_perform_async_0,);
define_aot_perform_async!(nulang_aot_perform_async_1, a0);
define_aot_perform_async!(nulang_aot_perform_async_2, a0, a1);
define_aot_perform_async!(nulang_aot_perform_async_3, a0, a1, a2);
define_aot_perform_async!(nulang_aot_perform_async_4, a0, a1, a2, a3);
define_aot_perform_async!(nulang_aot_perform_async_5, a0, a1, a2, a3, a4);
define_aot_perform_async!(nulang_aot_perform_async_6, a0, a1, a2, a3, a4, a5);
define_aot_perform_async!(nulang_aot_perform_async_7, a0, a1, a2, a3, a4, a5, a6);
define_aot_perform_async!(nulang_aot_perform_async_8, a0, a1, a2, a3, a4, a5, a6, a7);

// ---------------------------------------------------------------------------
// AOT workflow signal wait
// ---------------------------------------------------------------------------
// `perform Signal.wait("name")` lowers to a `nulang_aot_signal_wait` call.
// The helper resolves the signal name from the module pool and routes through
// the current callbacks' `wait_signal` (the same path the bytecode SignalWait
// opcode takes): a ready signal delivers its value, and outside a workflow
// context the default callback delivers unit. A signal that has not been
// received degrades to nil — the native backend has no VM suspension to park
// the actor mid-behavior.

// ---------------------------------------------------------------------------
// AOT actor migration
// ---------------------------------------------------------------------------
// `migrate actor to node` lowers to a `nulang_aot_migrate` call. The native
// backend has no distribution layer, so the request is a no-op that delivers
// unit — the same contract as the bytecode VM without distributed callbacks
// armed (which records the request but cannot act on it).

#[no_mangle]
pub unsafe extern "C" fn nulang_aot_migrate(_actor_raw: u64, _node_raw: u64) -> u64 {
    Value::unit().as_raw()
}

#[no_mangle]
pub unsafe extern "C" fn nulang_aot_signal_wait(name_raw: u64) -> u64 {
    let name = resolve_string_coerce(name_raw).unwrap_or_default();
    match try_with_callbacks(|cb| cb.wait_signal(&name)) {
        Some(crate::vm::SignalWaitResult::Ready(v)) => v.as_raw(),
        // NotReady or unarmed callbacks degrade to nil (no native suspension).
        _ => Value::nil().as_raw(),
    }
}

// ---------------------------------------------------------------------------
// AOT selective receive
// ---------------------------------------------------------------------------
// `receive { | Behavior(params) => ... }` in an AOT-compiled behavior lowers
// to an arity-matched `nulang_aot_receive_match_N` call with the candidate
// behavior ids as raw u64s. The helper scans the current actor's mailbox (via
// the callbacks' `try_receive_match`), returns the matched arm index as a
// boxed Int — or the arm count when nothing matched, mirroring the VM's
// ReceiveMatch contract — and stashes the payload for
// `nulang_aot_receive_payload`, which the codegen calls once per parameter
// slot. Timed receive (`after ms`) behaves as untimed in AOT: no suspension.

thread_local! {
    /// Payload of the most recent AOT receive match, read by
    /// `nulang_aot_receive_payload`.
    static AOT_RECEIVE_PAYLOAD: std::cell::RefCell<Vec<Value>> =
        std::cell::RefCell::new(Vec::new());
}

thread_local! {
    /// Pending `(name_const_idx, value)` init pairs for the next AOT spawn,
    /// pushed by `nulang_aot_spawn_push` and drained by `nulang_aot_spawn`.
    static AOT_SPAWN_INIT: std::cell::RefCell<Vec<(u64, Value)>> =
        std::cell::RefCell::new(Vec::new());
}

/// Queue one `(name, value)` init pair for the next `nulang_aot_spawn`.
/// `name_idx` is the position of the field name in the module constant pool.
#[no_mangle]
pub unsafe extern "C" fn nulang_aot_spawn_push(name_idx: u64, value: u64) {
    AOT_SPAWN_INIT.with(|c| {
        c.borrow_mut().push((name_idx, Value::from_bits(value)));
    });
}

/// Drain the queued spawn init pairs (used by `aot::nulang_aot_spawn`).
pub fn take_aot_spawn_init() -> Vec<(u64, Value)> {
    AOT_SPAWN_INIT.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

macro_rules! define_aot_receive {
    ($name:ident, $($id:ident),*) => {
        /// Selective receive from AOT-compiled code.
        #[no_mangle]
        pub unsafe extern "C" fn $name($($id: u64),*) -> u64 {
            let ids: Vec<u16> = vec![$($id as u16),*];
            match try_with_callbacks(|cb| cb.try_receive_match(&ids)).flatten() {
                Some((idx, payload)) => {
                    AOT_RECEIVE_PAYLOAD.with(|c| *c.borrow_mut() = payload);
                    Value::int(idx as i64).as_raw()
                }
                None => Value::int(ids.len() as i64).as_raw(),
            }
        }
    };
}

define_aot_receive!(nulang_aot_receive_match_1, id0);
define_aot_receive!(nulang_aot_receive_match_2, id0, id1);
define_aot_receive!(nulang_aot_receive_match_3, id0, id1, id2);
define_aot_receive!(nulang_aot_receive_match_4, id0, id1, id2, id3);
define_aot_receive!(nulang_aot_receive_match_5, id0, id1, id2, id3, id4);
define_aot_receive!(nulang_aot_receive_match_6, id0, id1, id2, id3, id4, id5);
define_aot_receive!(
    nulang_aot_receive_match_7,
    id0,
    id1,
    id2,
    id3,
    id4,
    id5,
    id6
);
define_aot_receive!(
    nulang_aot_receive_match_8,
    id0,
    id1,
    id2,
    id3,
    id4,
    id5,
    id6,
    id7
);

/// Read the i-th payload value of the most recent AOT receive match (boxed),
/// or nil when out of range.
#[no_mangle]
pub unsafe extern "C" fn nulang_aot_receive_payload(idx: u64) -> u64 {
    AOT_RECEIVE_PAYLOAD.with(|c| {
        c.borrow()
            .get(idx as usize)
            .map(|v| v.as_raw())
            .unwrap_or_else(|| Value::nil().as_raw())
    })
}

/// Legacy pop-any receive (`RValue::Receive`): pops the next mailbox message
/// and returns its first payload value (boxed), or nil when the mailbox is
/// empty or no actor is active. The behavior id is discarded — the MIR
/// contract only consumes the first payload value.
#[no_mangle]
pub unsafe extern "C" fn nulang_aot_receive_pop() -> u64 {
    try_with_callbacks(|cb| cb.try_receive())
        .flatten()
        .map(|(_, first)| first.as_raw())
        .unwrap_or_else(|| Value::nil().as_raw())
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_jit_status_slots_are_thread_local() {
        let left = std::thread::spawn(|| {
            let mut budget = 1u64;
            super::set_jit_safepoint_ptr(&mut budget);
            assert_eq!(super::nulang_jit_safepoint_check(0), 1);
            super::nulang_jit_set_yield_pc(7);
            super::nulang_jit_set_branch_exit_pc(11);
            assert_eq!(super::take_jit_yield_pc(), Some(7));
            assert_eq!(super::take_jit_branch_exit_pc(), Some(11));
            super::clear_jit_safepoint_ptr();
        });
        let right = std::thread::spawn(|| {
            let mut budget = 100u64;
            super::set_jit_safepoint_ptr(&mut budget);
            assert_eq!(super::nulang_jit_safepoint_check(0), 0);
            assert_eq!(super::take_jit_yield_pc(), None);
            assert_eq!(super::take_jit_branch_exit_pc(), None);
            super::clear_jit_safepoint_ptr();
        });
        left.join().unwrap();
        right.join().unwrap();
    }

    #[test]
    fn test_jit_helpers_linked() {
        // Force the linker to retain the JIT runtime helpers by taking
        // their addresses. Without this, the linker may strip them since
        // they are only called from JIT-compiled code.
        let _ = super::nulang_arr_store as unsafe extern "C" fn(_, _, _, _);
        let _ = super::nulang_arr_len as unsafe extern "C" fn(_, _, _);
        let _ = super::nulang_field_load as unsafe extern "C" fn(_, _, _, _);
        let _ = super::nulang_safepoint_yield as unsafe extern "C" fn(u64) -> u64;
        let _ = super::nulang_jit_safepoint_check as extern "C" fn(u64) -> u64;
        let _ = super::nulang_jit_set_yield_pc as extern "C" fn(u64) -> u64;
        let _ = super::nulang_jit_set_branch_exit_pc as extern "C" fn(u64) -> u64;
        let _ = super::nulang_alloc_obj as unsafe extern "C" fn(u64, u32) -> u64;
        let _ = super::nulang_obj_get as unsafe extern "C" fn(u64, u64) -> u64;
        let _ = super::nulang_obj_set as unsafe extern "C" fn(u64, u64, u64);
        let _ = super::nulang_obj_len as unsafe extern "C" fn(u64) -> u64;
        let _ = super::nulang_rec_copy as unsafe extern "C" fn(u64) -> u64;
        let _ = super::nulang_str_eq as unsafe extern "C" fn(u64, u64) -> u64;
        let _ = super::nulang_str_concat as unsafe extern "C" fn(u64, u64) -> u64;
        let _ = super::nulang_pow as extern "C" fn(u64, u64) -> u64;
        let _ = super::nulang_aot_self_ref as unsafe extern "C" fn() -> u64;
        let _ = super::nulang_aot_state_get as unsafe extern "C" fn(u64) -> u64;
        let _ = super::nulang_aot_state_set as unsafe extern "C" fn(u64, u64);
        let _ = super::nulang_aot_send_0 as unsafe extern "C" fn(u64, u64);
        let _ = super::nulang_aot_send_1 as unsafe extern "C" fn(u64, u64, u64);
        let _ = super::nulang_aot_send_2 as unsafe extern "C" fn(u64, u64, u64, u64);
        let _ = super::nulang_aot_send_8
            as unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64);
        let _ = super::nulang_aot_receive_match_1 as unsafe extern "C" fn(u64) -> u64;
        let _ = super::nulang_aot_receive_match_2 as unsafe extern "C" fn(u64, u64) -> u64;
        let _ = super::nulang_aot_receive_payload as unsafe extern "C" fn(u64) -> u64;
        let _ = super::nulang_aot_receive_pop as unsafe extern "C" fn() -> u64;
        let _ = super::nulang_aot_spawn_push as unsafe extern "C" fn(u64, u64);
        let _ = super::nulang_aot_emit_0 as unsafe extern "C" fn(u64);
        let _ = super::nulang_aot_emit_1 as unsafe extern "C" fn(u64, u64);
        let _ = super::nulang_aot_emit_8
            as unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64);
    }

    #[test]
    fn test_str_concat_and_iadd_string_coercion() {
        // Set up the standalone AOT heap + constant pool so the helpers can
        // resolve a TAG_STRING constant and allocate into the heap.
        let mut heap = crate::runtime::heap::ActorHeap::new(1024 * 1024);
        heap.set_actor_id(0);
        super::aot_set_heap(heap);
        unsafe {
            super::aot_set_constants(&[crate::bytecode::Constant::String("hello".into())]);
        }
        let hello = crate::vm::Value::string(0).as_raw();

        // nulang_str_concat must coerce a non-string operand: "hello" + 2 = "hello2".
        let r1 = unsafe { super::nulang_str_concat(hello, crate::vm::Value::int(2).as_raw()) };
        assert_eq!(super::resolve_string_coerce(r1).as_deref(), Some("hello2"));

        // nulang_iadd must also handle string operands (unknown-type add):
        // "hello" + 2 + 3 = "hello23".
        let r2 = super::nulang_iadd(hello, crate::vm::Value::int(2).as_raw());
        let r3 = super::nulang_iadd(r2, crate::vm::Value::int(3).as_raw());
        assert_eq!(super::resolve_string_coerce(r3).as_deref(), Some("hello23"));

        // Pure int add is unaffected.
        let ri = super::nulang_iadd(
            crate::vm::Value::int(5).as_raw(),
            crate::vm::Value::int(7).as_raw(),
        );
        assert_eq!(crate::vm::Value::from_raw(ri).as_int(), Some(12));

        super::aot_clear_constants();
        let _ = super::aot_take_heap();
    }
}
