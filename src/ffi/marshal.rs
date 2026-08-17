//! Marshalling between Nulang `Value`s and C ABI types.

use std::ffi::{c_char, c_void, CStr, CString};

use crate::bytecode::FfiType;
use crate::vm::Value;

use super::native::NativeFunction;

/// C ABI types supported by the FFI layer.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CType {
    I64,
    F64,
    Bool,
    CStr,
    VoidPtr,
    Unit,
}

/// A C function signature for marshalling.
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub params: Vec<CType>,
    pub ret: CType,
}

impl Signature {
    pub fn new(params: Vec<CType>, ret: CType) -> Self {
        Self { params, ret }
    }
}

/// Convert a bytecode FFI type to the runtime C type used for marshalling.
pub fn ffi_type_to_ctype(t: &FfiType) -> Option<CType> {
    match t {
        FfiType::Int => Some(CType::I64),
        FfiType::Float => Some(CType::F64),
        FfiType::Bool => Some(CType::Bool),
        FfiType::String => Some(CType::CStr),
        FfiType::Unit => Some(CType::Unit),
        FfiType::Pointer => Some(CType::VoidPtr),
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers: Value -> C argument
// ---------------------------------------------------------------------------

/// Extract an `i64` from a Nulang value.
pub fn value_to_i64(v: &Value) -> Result<i64, String> {
    v.as_int().ok_or_else(|| "expected int".to_string())
}

/// Extract an `f64` from a Nulang value.
pub fn value_to_f64(v: &Value) -> Result<f64, String> {
    v.as_float().ok_or_else(|| "expected float".to_string())
}

/// Extract a `bool` from a Nulang value.
pub fn value_to_bool(v: &Value) -> Result<bool, String> {
    v.as_bool().ok_or_else(|| "expected bool".to_string())
}

/// Extract a C string pointer from a Nulang pointer value.
///
/// # Safety
/// The returned pointer is borrowed from the value and must remain valid for
/// the duration of the native call.
pub unsafe fn value_to_cstr(v: &Value) -> Result<*const c_char, String> {
    v.as_ptr()
        .ok_or_else(|| "expected pointer string".to_string())
        .map(|p| p as *const c_char)
}

/// Extract a void pointer from a Nulang pointer value.
///
/// # Safety
/// The returned pointer is borrowed from the value and must remain valid for
/// the duration of the native call.
pub unsafe fn value_to_voidptr(v: &Value) -> Result<*mut c_void, String> {
    v.as_ptr()
        .ok_or_else(|| "expected pointer".to_string())
        .map(|p| p as *mut c_void)
}

// ---------------------------------------------------------------------------
// Conversion helpers: C return value -> Value
// ---------------------------------------------------------------------------

/// Marshal a C `i64` return value into a Nulang value.
pub fn i64_to_value(n: i64) -> Value {
    Value::int(n)
}

/// Marshal a C `f64` return value into a Nulang value.
pub fn f64_to_value(f: f64) -> Value {
    Value::float(f)
}

/// Marshal a C `bool` return value into a Nulang value.
pub fn bool_to_value(b: bool) -> Value {
    Value::bool(b)
}

/// Pack a raw pointer into a Nulang pointer value, returning `Value::nil()`
/// when the address does not fit in the 48-bit payload (e.g. LA57 or
/// AArch64-52 virtual addresses). Packing a truncated address would produce
/// a dangling pointer that is later dereferenced as if valid, so failing
/// closed here is strictly safer than the legacy masking behavior.
fn ptr_to_value_checked(p: *mut u8) -> Value {
    if crate::value_layout::ptr_fits_payload(p as u64) {
        Value::ptr(p)
    } else {
        Value::nil()
    }
}

/// Marshal a C string return value into a Nulang pointer value.
///
/// The string is copied into a `CString` and the pointer is leaked to the VM
/// heap model. The caller is responsible for freeing the returned pointer with
/// `free_cstr_value` once it is copied into the actor heap.
///
/// # Safety
/// `s` must be a valid, null-terminated C string.
pub unsafe fn cstr_to_value(s: *const c_char) -> Value {
    if s.is_null() {
        return Value::nil();
    }
    // SAFETY: caller guarantees `s` is a valid, null-terminated C string.
    let cstr = CStr::from_ptr(s);
    let cstring = CString::new(cstr.to_bytes()).unwrap_or_else(|_| CString::default());
    ptr_to_value_checked(cstring.into_raw() as *mut u8)
}

/// Free a pointer value previously created by `cstr_to_value`.
///
/// # Safety
/// `v` must be a pointer value whose payload was returned by `CString::into_raw`.
pub unsafe fn free_cstr_value(v: Value) {
    if let Some(ptr) = v.as_ptr() {
        // SAFETY: ptr came from CString::into_raw in cstr_to_value.
        let _ = CString::from_raw(ptr as *mut c_char);
    }
}

/// Marshal a C void pointer return value into a Nulang pointer value.
pub fn voidptr_to_value(p: *mut c_void) -> Value {
    if p.is_null() {
        Value::nil()
    } else {
        ptr_to_value_checked(p as *mut u8)
    }
}

/// Marshal a C unit return value into a Nulang unit value.
pub fn unit_to_value() -> Value {
    Value::unit()
}

// ---------------------------------------------------------------------------
// Type-driven dispatch trait
// ---------------------------------------------------------------------------

/// Maps a supported C type to its Rust FFI representation and provides
/// conversions to/from Nulang `Value`.
pub trait CTypeArg: Copy {
    /// The Rust type used in an `extern "C" fn` signature.
    type Abi: Copy;
    /// The corresponding `CType` variant.
    const CTYPE: CType;
    /// Convert from a Nulang `Value` to this argument type.
    fn from_value(v: Value) -> Result<Self, String>;
    /// Convert this argument type to a Nulang `Value`.
    fn to_value(self) -> Value;
}

impl CTypeArg for i64 {
    type Abi = i64;
    const CTYPE: CType = CType::I64;
    fn from_value(v: Value) -> Result<Self, String> {
        value_to_i64(&v)
    }
    fn to_value(self) -> Value {
        i64_to_value(self)
    }
}

impl CTypeArg for f64 {
    type Abi = f64;
    const CTYPE: CType = CType::F64;
    fn from_value(v: Value) -> Result<Self, String> {
        value_to_f64(&v)
    }
    fn to_value(self) -> Value {
        f64_to_value(self)
    }
}

impl CTypeArg for bool {
    type Abi = bool;
    const CTYPE: CType = CType::Bool;
    fn from_value(v: Value) -> Result<Self, String> {
        value_to_bool(&v)
    }
    fn to_value(self) -> Value {
        bool_to_value(self)
    }
}

impl CTypeArg for *const c_char {
    type Abi = *const c_char;
    const CTYPE: CType = CType::CStr;
    fn from_value(v: Value) -> Result<Self, String> {
        // SAFETY: we only borrow the pointer for the duration of the call.
        unsafe { value_to_cstr(&v) }
    }
    // SAFETY: trait-impl signature is fixed; the pointer is a C string
    // produced by the FFI call whose signature declared it as CType::CStr.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn to_value(self) -> Value {
        // SAFETY: `self` is a C string pointer.
        unsafe { cstr_to_value(self) }
    }
}

impl CTypeArg for *mut c_void {
    type Abi = *mut c_void;
    const CTYPE: CType = CType::VoidPtr;
    fn from_value(v: Value) -> Result<Self, String> {
        // SAFETY: we only borrow the pointer for the duration of the call.
        unsafe { value_to_voidptr(&v) }
    }
    fn to_value(self) -> Value {
        voidptr_to_value(self)
    }
}

impl CTypeArg for () {
    type Abi = ();
    const CTYPE: CType = CType::Unit;
    fn from_value(_v: Value) -> Result<Self, String> {
        Ok(())
    }
    fn to_value(self) -> Value {
        unit_to_value()
    }
}

// ---------------------------------------------------------------------------
// Compile-time mapping from CType tokens to Rust ABI types.
// ---------------------------------------------------------------------------

/// Inject the return-type list into another macro invocation.
macro_rules! with_returns {
    ($macro:ident!($($args:tt)*)) => {
        $macro!($($args)*, [(I64, i64); (F64, f64); (Bool, bool); (CStr, *const std::ffi::c_char); (VoidPtr, *mut std::ffi::c_void); (Unit, ())])
    };
}

/// Generate match arms for arity 0 (no parameters).
macro_rules! arity_0_arms {
    ($ptr:expr, $ret:expr, [$(($r:ident, $rty:ty));*]) => {
        match $ret {
            $(CType::$r => {
                // SAFETY: caller guarantees the function ABI matches.
                let f: extern "C" fn() -> $rty = unsafe { std::mem::transmute($ptr) };
                Ok(<$rty as CTypeArg>::to_value(f()))
            },)*
        }
    };
}

/// Generate match arms for a single parameter of a fixed Rust type.
macro_rules! arity_1_arms {
    ($ptr:expr, $args:expr, $ret:expr, $pty:ty, [$(($r:ident, $rty:ty));*]) => {
        match $ret {
            $(CType::$r => {{
                // SAFETY: caller guarantees the function ABI matches.
                let f: extern "C" fn($pty) -> $rty = unsafe { std::mem::transmute($ptr) };
                let mut __iter = $args.iter();
                let __a0 = <$pty as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                Ok(<$rty as CTypeArg>::to_value(f(__a0)))
            }},)*
        }
    };
}

/// Generate match arms for two parameters of fixed Rust types.
macro_rules! arity_2_arms {
    ($ptr:expr, $args:expr, $ret:expr, $pty0:ty, $pty1:ty, [$(($r:ident, $rty:ty));*]) => {
        match $ret {
            $(CType::$r => {{
                // SAFETY: caller guarantees the function ABI matches.
                let f: extern "C" fn($pty0, $pty1) -> $rty = unsafe { std::mem::transmute($ptr) };
                let mut __iter = $args.iter();
                let __a0 = <$pty0 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                let __a1 = <$pty1 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                Ok(<$rty as CTypeArg>::to_value(f(__a0, __a1)))
            }},)*
        }
    };
}

/// Generate match arms for three parameters of fixed Rust types.
macro_rules! arity_3_arms {
    ($ptr:expr, $args:expr, $ret:expr, $pty0:ty, $pty1:ty, $pty2:ty, [$(($r:ident, $rty:ty));*]) => {
        match $ret {
            $(CType::$r => {{
                // SAFETY: caller guarantees the function ABI matches.
                let f: extern "C" fn($pty0, $pty1, $pty2) -> $rty = unsafe { std::mem::transmute($ptr) };
                let mut __iter = $args.iter();
                let __a0 = <$pty0 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                let __a1 = <$pty1 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                let __a2 = <$pty2 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                Ok(<$rty as CTypeArg>::to_value(f(__a0, __a1, __a2)))
            }},)*
        }
    };
}

/// Generate match arms for four parameters of fixed Rust types.
macro_rules! arity_4_arms {
    ($ptr:expr, $args:expr, $ret:expr, $pty0:ty, $pty1:ty, $pty2:ty, $pty3:ty, [$(($r:ident, $rty:ty));*]) => {
        match $ret {
            $(CType::$r => {{
                // SAFETY: caller guarantees the function ABI matches.
                let f: extern "C" fn($pty0, $pty1, $pty2, $pty3) -> $rty = unsafe { std::mem::transmute($ptr) };
                let mut __iter = $args.iter();
                let __a0 = <$pty0 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                let __a1 = <$pty1 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                let __a2 = <$pty2 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                let __a3 = <$pty3 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                Ok(<$rty as CTypeArg>::to_value(f(__a0, __a1, __a2, __a3)))
            }},)*
        }
    };
}

/// Marshal arguments, call a native function, and marshal the return value.
///
/// Supports signatures with up to four parameters.
///
/// # Safety
/// `func.ptr` must point to a valid function whose ABI matches `func.signature`.
pub unsafe fn call_native(func: &NativeFunction, args: &[Value]) -> Result<Value, String> {
    if args.len() != func.signature.params.len() {
        return Err(format!(
            "argument count mismatch: expected {}, got {}",
            func.signature.params.len(),
            args.len()
        ));
    }

    let p = &func.signature.params;
    let ret = func.signature.ret;

    match p.as_slice() {
        [] => with_returns!(arity_0_arms!(func.ptr, ret)),
        [p0] => match p0 {
            CType::I64 => with_returns!(arity_1_arms!(func.ptr, args, ret, i64)),
            CType::F64 => with_returns!(arity_1_arms!(func.ptr, args, ret, f64)),
            CType::Bool => with_returns!(arity_1_arms!(func.ptr, args, ret, bool)),
            CType::CStr => {
                with_returns!(arity_1_arms!(func.ptr, args, ret, *const std::ffi::c_char))
            }
            CType::VoidPtr => {
                with_returns!(arity_1_arms!(func.ptr, args, ret, *mut std::ffi::c_void))
            }
            CType::Unit => with_returns!(arity_1_arms!(func.ptr, args, ret, ())),
        },
        [p0, p1] => match (p0, p1) {
            // I64
            (CType::I64, CType::I64) => with_returns!(arity_2_arms!(func.ptr, args, ret, i64, i64)),
            (CType::I64, CType::F64) => with_returns!(arity_2_arms!(func.ptr, args, ret, i64, f64)),
            (CType::I64, CType::Bool) => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, i64, bool))
            }
            // F64
            (CType::F64, CType::I64) => with_returns!(arity_2_arms!(func.ptr, args, ret, f64, i64)),
            (CType::F64, CType::F64) => with_returns!(arity_2_arms!(func.ptr, args, ret, f64, f64)),
            (CType::F64, CType::Bool) => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, f64, bool))
            }
            // Bool
            (CType::Bool, CType::I64) => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, bool, i64))
            }
            (CType::Bool, CType::F64) => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, bool, f64))
            }
            (CType::Bool, CType::Bool) => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, bool, bool))
            }
            _ => Err("unsupported arity-2 parameter types".to_string()),
        },
        [p0, p1, p2] => match (p0, p1, p2) {
            (CType::I64, CType::I64, CType::I64) => {
                with_returns!(arity_3_arms!(func.ptr, args, ret, i64, i64, i64))
            }
            _ => {
                Err("unsupported arity-3 parameter types (only I64,I64,I64 supported)".to_string())
            }
        },
        [p0, p1, p2, p3] => match (p0, p1, p2, p3) {
            (CType::I64, CType::I64, CType::I64, CType::I64) => {
                with_returns!(arity_4_arms!(func.ptr, args, ret, i64, i64, i64, i64))
            }
            _ => Err("unsupported arity-4 parameter types (only I64x4 supported)".to_string()),
        },
        _ => Err(format!(
            "unsupported parameter count: {} (max 4 supported)",
            p.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::native::NativeLibrary;
    use super::*;
    use std::ffi::CString;

    extern "C" fn add_two(a: i64, b: i64) -> i64 {
        a + b
    }

    extern "C" fn negate_f(x: f64) -> f64 {
        -x
    }

    extern "C" fn echo_bool(b: bool) -> bool {
        !b
    }

    extern "C" fn strlen_c(s: *const c_char) -> i64 {
        if s.is_null() {
            return 0;
        }
        // SAFETY: test strings are valid null-terminated C strings.
        unsafe { CStr::from_ptr(s).to_bytes().len() as i64 }
    }

    extern "C" fn return_unit() {}

    extern "C" fn sum_three(a: i64, b: i64, c: i64) -> i64 {
        a + b + c
    }

    fn make_func(ptr: *const c_void, signature: Signature) -> NativeFunction {
        NativeFunction {
            ptr,
            signature,
            library: None,
            symbol: "test".to_string(),
        }
    }

    #[test]
    fn test_call_native_i64_add() {
        let func = make_func(
            add_two as *const c_void,
            Signature::new(vec![CType::I64, CType::I64], CType::I64),
        );
        let args = [Value::int(3), Value::int(5)];
        // SAFETY: pointer matches signature.
        let result = unsafe { call_native(&func, &args).unwrap() };
        assert_eq!(result.as_int(), Some(8));
    }

    #[test]
    fn test_marshal_i64_roundtrip() {
        let v = Value::int(-42);
        assert_eq!(value_to_i64(&v), Ok(-42));
        assert_eq!(i64_to_value(-42).as_int(), Some(-42));
    }

    #[test]
    fn test_marshal_f64_roundtrip() {
        let v = Value::float(2.5);
        assert_eq!(value_to_f64(&v), Ok(2.5));
        assert_eq!(f64_to_value(2.5).as_float(), Some(2.5));
    }

    #[test]
    fn test_marshal_bool_roundtrip() {
        let v = Value::bool(true);
        assert_eq!(value_to_bool(&v), Ok(true));
        assert!(!bool_to_value(false).as_bool().unwrap());
    }

    #[test]
    fn test_marshal_cstr_roundtrip() {
        let original = CString::new("hello ffi").unwrap();
        let ptr = original.as_ptr() as *mut u8;
        let v = Value::ptr(ptr);
        // SAFETY: pointer is a valid C string for the borrow.
        let borrowed = unsafe { value_to_cstr(&v).unwrap() };
        // SAFETY: borrowed pointer is valid.
        let round = unsafe { cstr_to_value(borrowed) };
        let round_ptr = round.as_ptr().unwrap() as *const c_char;
        // SAFETY: round pointer is a valid C string.
        assert_eq!(
            unsafe { CStr::from_ptr(round_ptr).to_str().unwrap() },
            "hello ffi"
        );
    }

    #[test]
    fn test_marshal_voidptr_roundtrip() {
        let mut n: i64 = 123;
        let p = &mut n as *mut i64 as *mut c_void;
        let v = voidptr_to_value(p);
        // SAFETY: pointer is valid.
        let p2 = unsafe { value_to_voidptr(&v).unwrap() } as *mut i64;
        // SAFETY: p2 points to valid i64.
        assert_eq!(unsafe { *p2 }, 123);
    }

    #[test]
    fn test_marshal_unit() {
        let v = unit_to_value();
        assert!(v.is_unit());
        let u = <() as CTypeArg>::from_value(v).unwrap();
        assert_eq!(u, ());
    }

    #[test]
    fn test_call_native_float() {
        let func = make_func(
            negate_f as *const c_void,
            Signature::new(vec![CType::F64], CType::F64),
        );
        // SAFETY: pointer matches signature.
        let result = unsafe { call_native(&func, &[Value::float(2.5)]).unwrap() };
        assert_eq!(result.as_float(), Some(-2.5));
    }

    #[test]
    fn test_call_native_bool() {
        let func = make_func(
            echo_bool as *const c_void,
            Signature::new(vec![CType::Bool], CType::Bool),
        );
        // SAFETY: pointer matches signature.
        let result = unsafe { call_native(&func, &[Value::bool(true)]).unwrap() };
        assert_eq!(result.as_bool(), Some(false));
    }

    #[test]
    fn test_call_native_cstr() {
        let func = make_func(
            strlen_c as *const c_void,
            Signature::new(vec![CType::CStr], CType::I64),
        );
        let s = CString::new("nulang").unwrap();
        let v = Value::ptr(s.as_ptr() as *mut u8);
        // SAFETY: pointer matches signature and is a valid C string.
        let result = unsafe { call_native(&func, &[v]).unwrap() };
        assert_eq!(result.as_int(), Some(6));
    }

    #[test]
    fn test_call_native_unit_ret() {
        let func = make_func(
            return_unit as *const c_void,
            Signature::new(vec![], CType::Unit),
        );
        // SAFETY: pointer matches signature.
        let result = unsafe { call_native(&func, &[]).unwrap() };
        assert!(result.is_unit());
    }

    #[test]
    fn test_call_native_three_args() {
        let func = make_func(
            sum_three as *const c_void,
            Signature::new(vec![CType::I64, CType::I64, CType::I64], CType::I64),
        );
        // SAFETY: pointer matches signature.
        let result =
            unsafe { call_native(&func, &[Value::int(1), Value::int(2), Value::int(3)]).unwrap() };
        assert_eq!(result.as_int(), Some(6));
    }

    #[test]
    fn test_call_native_argument_count_mismatch() {
        let func = make_func(
            add_two as *const c_void,
            Signature::new(vec![CType::I64, CType::I64], CType::I64),
        );
        // SAFETY: call itself is safe; we only check the error it returns.
        let result = unsafe { call_native(&func, &[Value::int(1)]) };
        assert!(result.is_err());
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "ffi"))]
    fn test_load_libm_sqrt() {
        // SAFETY: libm.so.6 is a trusted system library.
        let lib = unsafe { NativeLibrary::open("libm.so.6") };
        if let Err(e) = &lib {
            eprintln!("warning: could not open libm.so.6: {}", e);
            return;
        }
        let lib = lib.unwrap();
        // SAFETY: sqrt has the expected signature; `resolve` returns the
        // pointer libloading resolved for the requested `T`.
        let ptr = unsafe { lib.resolve::<extern "C" fn(f64) -> f64>(b"sqrt\0").unwrap() };
        let sqrt: extern "C" fn(f64) -> f64 = unsafe { std::mem::transmute(ptr) };
        assert!((sqrt(4.0) - 2.0).abs() < 1e-12);
    }
}

/// Map a Nulang type to its FFI representation.
/// Moved from compiler.rs; shared with the MIR codegen.
pub(crate) fn nulang_type_to_ffi_type(ty: &crate::types::Type) -> Option<crate::bytecode::FfiType> {
    use crate::bytecode::FfiType;
    use crate::types::Type;
    match ty {
        Type::Primitive(p) => match p {
            crate::types::PrimitiveType::Int => Some(FfiType::Int),
            crate::types::PrimitiveType::Float => Some(FfiType::Float),
            crate::types::PrimitiveType::Bool => Some(FfiType::Bool),
            crate::types::PrimitiveType::String => Some(FfiType::String),
            crate::types::PrimitiveType::Unit => Some(FfiType::Unit),
            _ => None,
        },
        _ => None,
    }
}
