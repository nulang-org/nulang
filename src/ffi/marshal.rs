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
    /// A raw Nulang value passed as an opaque i64-tagged value. This is only
    /// usable via the C registration API; it has no language-level syntax.
    Value,
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
        FfiType::Value => Some(CType::Value),
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
// libffi-based dynamic invocation (used when the `ffi` feature is enabled)
// ---------------------------------------------------------------------------

#[cfg(feature = "ffi")]
fn ctype_to_libffi(ty: &CType) -> libffi::middle::Type {
    use libffi::middle::Type;
    match ty {
        CType::I64 => Type::i64(),
        CType::F64 => Type::f64(),
        CType::Bool => Type::u8(),
        CType::CStr => Type::pointer(),
        CType::VoidPtr => Type::pointer(),
        CType::Unit => Type::void(),
        CType::Value => Type::u64(),
    }
}

#[cfg(feature = "ffi")]
/// Marshal arguments, call a native function, and marshal the return value.
///
/// Uses libffi so arbitrary arities and argument type combinations are
/// supported, not only the small fixed table used by the non-ffi fallback.
///
/// # Safety
/// `func.ptr` must point to a valid function whose ABI matches `func.signature`.
pub unsafe fn call_native(func: &NativeFunction, args: &[Value]) -> Result<Value, String> {
    use libffi::middle::{arg, Cif, CodePtr, Type};

    if args.len() != func.signature.params.len() {
        return Err(format!(
            "argument count mismatch: expected {}, got {}",
            func.signature.params.len(),
            args.len()
        ));
    }

    // Storage vectors keep argument values alive so libffi can borrow pointers
    // to them for the duration of the call.
    let mut i64_storage: Vec<i64> = Vec::new();
    let mut f64_storage: Vec<f64> = Vec::new();
    let mut u8_storage: Vec<u8> = Vec::new();
    let mut ptr_storage: Vec<*const c_void> = Vec::new();
    let mut u64_storage: Vec<u64> = Vec::new();

    let mut ffi_types: Vec<Type> = Vec::with_capacity(args.len());
    /// Tracks which storage vector holds each argument, so `ffi_args` can be
    /// built after all pushes complete (avoiding overlapping mutable/immutable
    /// borrows of the storage vectors).
    enum ArgSlot {
        I64(usize),
        F64(usize),
        U8(usize),
        Ptr(usize),
        U64(usize),
    }
    let mut arg_slots: Vec<ArgSlot> = Vec::with_capacity(args.len());

    for (i, (ctype, val)) in func.signature.params.iter().zip(args.iter()).enumerate() {
        match ctype {
            CType::I64 => {
                i64_storage.push(
                    value_to_i64(val)
                        .map_err(|e| format!("FFI argument {} for {}: {}", i, func.symbol, e))?,
                );
                ffi_types.push(Type::i64());
                arg_slots.push(ArgSlot::I64(i64_storage.len() - 1));
            }
            CType::F64 => {
                f64_storage.push(
                    value_to_f64(val)
                        .map_err(|e| format!("FFI argument {} for {}: {}", i, func.symbol, e))?,
                );
                ffi_types.push(Type::f64());
                arg_slots.push(ArgSlot::F64(f64_storage.len() - 1));
            }
            CType::Bool => {
                u8_storage.push(
                    if value_to_bool(val)
                        .map_err(|e| format!("FFI argument {} for {}: {}", i, func.symbol, e))?
                    {
                        1
                    } else {
                        0
                    },
                );
                ffi_types.push(Type::u8());
                arg_slots.push(ArgSlot::U8(u8_storage.len() - 1));
            }
            CType::CStr => {
                let p = unsafe { value_to_cstr(val) }
                    .map_err(|e| format!("FFI argument {} for {}: {}", i, func.symbol, e))?;
                ptr_storage.push(p as *const c_void);
                ffi_types.push(Type::pointer());
                arg_slots.push(ArgSlot::Ptr(ptr_storage.len() - 1));
            }
            CType::VoidPtr => {
                let p = unsafe { value_to_voidptr(val) }
                    .map_err(|e| format!("FFI argument {} for {}: {}", i, func.symbol, e))?;
                ptr_storage.push(p as *const c_void);
                ffi_types.push(Type::pointer());
                arg_slots.push(ArgSlot::Ptr(ptr_storage.len() - 1));
            }
            CType::Unit => {
                u8_storage.push(0);
                ffi_types.push(Type::u8());
                arg_slots.push(ArgSlot::U8(u8_storage.len() - 1));
            }
            CType::Value => {
                u64_storage.push(val.to_bits());
                ffi_types.push(Type::u64());
                arg_slots.push(ArgSlot::U64(u64_storage.len() - 1));
            }
        }
    }

    let mut ffi_args: Vec<libffi::middle::Arg> = Vec::with_capacity(args.len());
    for slot in arg_slots {
        match slot {
            ArgSlot::I64(i) => ffi_args.push(arg(&i64_storage[i])),
            ArgSlot::F64(i) => ffi_args.push(arg(&f64_storage[i])),
            ArgSlot::U8(i) => ffi_args.push(arg(&u8_storage[i])),
            ArgSlot::Ptr(i) => ffi_args.push(arg(&ptr_storage[i])),
            ArgSlot::U64(i) => ffi_args.push(arg(&u64_storage[i])),
        }
    }

    let ret_type = ctype_to_libffi(&func.signature.ret);
    let cif = Cif::new(ffi_types, ret_type);
    let code = CodePtr(func.ptr as *mut c_void);

    match func.signature.ret {
        CType::I64 => {
            let r: i64 = unsafe { cif.call(code, &ffi_args) };
            Ok(i64_to_value(r))
        }
        CType::F64 => {
            let r: f64 = unsafe { cif.call(code, &ffi_args) };
            Ok(f64_to_value(r))
        }
        CType::Bool => {
            let r: u8 = unsafe { cif.call(code, &ffi_args) };
            Ok(bool_to_value(r != 0))
        }
        CType::CStr => {
            let r: *const c_char = unsafe { cif.call(code, &ffi_args) };
            // SAFETY: caller guarantees the returned pointer is a valid C
            // string (or null) for the duration of this conversion.
            Ok(unsafe { cstr_to_value(r) })
        }
        CType::VoidPtr => {
            let r: *mut c_void = unsafe { cif.call(code, &ffi_args) };
            Ok(voidptr_to_value(r))
        }
        CType::Unit => {
            let _: () = unsafe { cif.call(code, &ffi_args) };
            Ok(unit_to_value())
        }
        CType::Value => {
            let r: u64 = unsafe { cif.call(code, &ffi_args) };
            Ok(Value::from_bits(r))
        }
    }
}

// ---------------------------------------------------------------------------
// Fixed-arity fallback (used when the `ffi` feature is disabled)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "ffi"))]
mod fixed_arity {
    use super::*;

    /// Maps a supported C type to its Rust FFI representation and provides
    /// conversions to/from Nulang `Value`.
    pub trait CTypeArg: Copy {
        /// The Rust type used in an `extern "C" fn` signature.
        type Abi: Copy;
        /// Convert from a Nulang `Value` to this argument type.
        fn from_value(v: Value) -> Result<Self, String>;
        /// Convert this argument type to a Nulang `Value`.
        fn to_value(self) -> Value;
    }

    impl CTypeArg for i64 {
        type Abi = i64;
        fn from_value(v: Value) -> Result<Self, String> {
            value_to_i64(&v)
        }
        fn to_value(self) -> Value {
            i64_to_value(self)
        }
    }
    impl CTypeArg for f64 {
        type Abi = f64;
        fn from_value(v: Value) -> Result<Self, String> {
            value_to_f64(&v)
        }
        fn to_value(self) -> Value {
            f64_to_value(self)
        }
    }
    impl CTypeArg for bool {
        type Abi = bool;
        fn from_value(v: Value) -> Result<Self, String> {
            value_to_bool(&v)
        }
        fn to_value(self) -> Value {
            bool_to_value(self)
        }
    }
    impl CTypeArg for *const c_char {
        type Abi = *const c_char;
        fn from_value(v: Value) -> Result<Self, String> {
            unsafe { value_to_cstr(&v) }
        }
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn to_value(self) -> Value {
            unsafe { cstr_to_value(self) }
        }
    }
    impl CTypeArg for *mut c_void {
        type Abi = *mut c_void;
        fn from_value(v: Value) -> Result<Self, String> {
            unsafe { value_to_voidptr(&v) }
        }
        fn to_value(self) -> Value {
            voidptr_to_value(self)
        }
    }
    impl CTypeArg for () {
        type Abi = ();
        fn from_value(_v: Value) -> Result<Self, String> {
            Ok(())
        }
        fn to_value(self) -> Value {
            unit_to_value()
        }
    }

    macro_rules! with_returns {
        ($macro:ident!($($args:tt)*)) => {
            $macro!($($args)*, [(I64, i64); (F64, f64); (Bool, bool); (CStr, *const std::ffi::c_char); (VoidPtr, *mut std::ffi::c_void); (Unit, ())])
        };
    }

    macro_rules! arity_0_arms {
        ($ptr:expr, $ret:expr, [$(($r:ident, $rty:ty));*]) => {
            match $ret {
                $(CType::$r => {
                    let f: extern "C" fn() -> $rty = unsafe { std::mem::transmute($ptr) };
                    Ok(<$rty as CTypeArg>::to_value(f()))
                },)*
                _ => Err("unsupported native return type (Value requires the ffi feature)".to_string()),
            }
        };
    }
    macro_rules! arity_1_arms {
        ($ptr:expr, $args:expr, $ret:expr, $pty:ty, [$(($r:ident, $rty:ty));*]) => {
            match $ret {
                $(CType::$r => {{
                    let f: extern "C" fn($pty) -> $rty = unsafe { std::mem::transmute($ptr) };
                    let mut __iter = $args.iter();
                    let __a0 = <$pty as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    Ok(<$rty as CTypeArg>::to_value(f(__a0)))
                }},)*
                _ => Err("unsupported native return type (Value requires the ffi feature)".to_string()),
            }
        };
    }
    macro_rules! arity_2_arms {
        ($ptr:expr, $args:expr, $ret:expr, $pty0:ty, $pty1:ty, [$(($r:ident, $rty:ty));*]) => {
            match $ret {
                $(CType::$r => {{
                    let f: extern "C" fn($pty0, $pty1) -> $rty = unsafe { std::mem::transmute($ptr) };
                    let mut __iter = $args.iter();
                    let __a0 = <$pty0 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    let __a1 = <$pty1 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    Ok(<$rty as CTypeArg>::to_value(f(__a0, __a1)))
                }},)*
                _ => Err("unsupported native return type (Value requires the ffi feature)".to_string()),
            }
        };
    }
    macro_rules! arity_3_arms {
        ($ptr:expr, $args:expr, $ret:expr, $pty0:ty, $pty1:ty, $pty2:ty, [$(($r:ident, $rty:ty));*]) => {
            match $ret {
                $(CType::$r => {{
                    let f: extern "C" fn($pty0, $pty1, $pty2) -> $rty = unsafe { std::mem::transmute($ptr) };
                    let mut __iter = $args.iter();
                    let __a0 = <$pty0 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    let __a1 = <$pty1 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    let __a2 = <$pty2 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    Ok(<$rty as CTypeArg>::to_value(f(__a0, __a1, __a2)))
                }},)*
                _ => Err("unsupported native return type (Value requires the ffi feature)".to_string()),
            }
        };
    }
    macro_rules! arity_4_arms {
        ($ptr:expr, $args:expr, $ret:expr, $pty0:ty, $pty1:ty, $pty2:ty, $pty3:ty, [$(($r:ident, $rty:ty));*]) => {
            match $ret {
                $(CType::$r => {{
                    let f: extern "C" fn($pty0, $pty1, $pty2, $pty3) -> $rty = unsafe { std::mem::transmute($ptr) };
                    let mut __iter = $args.iter();
                    let __a0 = <$pty0 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    let __a1 = <$pty1 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    let __a2 = <$pty2 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    let __a3 = <$pty3 as CTypeArg>::from_value(__iter.next().copied().unwrap_or(Value::nil()))?;
                    Ok(<$rty as CTypeArg>::to_value(f(__a0, __a1, __a2, __a3)))
                }},)*
                _ => Err("unsupported native return type (Value requires the ffi feature)".to_string()),
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
        if func.signature.params.iter().any(|p| *p == CType::Value)
            || func.signature.ret == CType::Value
        {
            return Err("CType::Value requires the ffi feature".to_string());
        }
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
            [CType::I64] => with_returns!(arity_1_arms!(func.ptr, args, ret, i64)),
            [CType::F64] => with_returns!(arity_1_arms!(func.ptr, args, ret, f64)),
            [CType::Bool] => with_returns!(arity_1_arms!(func.ptr, args, ret, bool)),
            [CType::CStr] => {
                with_returns!(arity_1_arms!(func.ptr, args, ret, *const std::ffi::c_char))
            }
            [CType::VoidPtr] => {
                with_returns!(arity_1_arms!(func.ptr, args, ret, *mut std::ffi::c_void))
            }
            [CType::Unit] => with_returns!(arity_1_arms!(func.ptr, args, ret, ())),
            [CType::I64, CType::I64] => with_returns!(arity_2_arms!(func.ptr, args, ret, i64, i64)),
            [CType::I64, CType::F64] => with_returns!(arity_2_arms!(func.ptr, args, ret, i64, f64)),
            [CType::I64, CType::Bool] => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, i64, bool))
            }
            [CType::F64, CType::I64] => with_returns!(arity_2_arms!(func.ptr, args, ret, f64, i64)),
            [CType::F64, CType::F64] => with_returns!(arity_2_arms!(func.ptr, args, ret, f64, f64)),
            [CType::F64, CType::Bool] => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, f64, bool))
            }
            [CType::Bool, CType::I64] => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, bool, i64))
            }
            [CType::Bool, CType::F64] => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, bool, f64))
            }
            [CType::Bool, CType::Bool] => {
                with_returns!(arity_2_arms!(func.ptr, args, ret, bool, bool))
            }
            [CType::I64, CType::I64, CType::I64] => {
                with_returns!(arity_3_arms!(func.ptr, args, ret, i64, i64, i64))
            }
            [CType::I64, CType::I64, CType::I64, CType::I64] => {
                with_returns!(arity_4_arms!(func.ptr, args, ret, i64, i64, i64, i64))
            }
            _ => Err(format!(
                "unsupported parameter count/types (max 4, no Value without ffi feature): {:?}",
                p
            )),
        }
    }
}

#[cfg(not(feature = "ffi"))]
pub use fixed_arity::call_native;

#[cfg(test)]
mod tests {
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
    }

    #[cfg(feature = "ffi")]
    extern "C" fn sum_six(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) -> i64 {
        a + b + c + d + e + f
    }

    #[cfg(feature = "ffi")]
    extern "C" fn identity_value(v: u64) -> u64 {
        v
    }

    #[cfg(feature = "ffi")]
    extern "C" fn make_greeting() -> *const c_char {
        let s = std::ffi::CString::new("hello ffi").unwrap();
        s.into_raw()
    }

    #[test]
    #[cfg(feature = "ffi")]
    fn test_call_native_six_args() {
        let func = make_func(
            sum_six as *const c_void,
            Signature::new(
                vec![
                    CType::I64,
                    CType::I64,
                    CType::I64,
                    CType::I64,
                    CType::I64,
                    CType::I64,
                ],
                CType::I64,
            ),
        );
        let result = unsafe {
            call_native(
                &func,
                &[
                    Value::int(1),
                    Value::int(2),
                    Value::int(3),
                    Value::int(4),
                    Value::int(5),
                    Value::int(6),
                ],
            )
            .unwrap()
        };
        assert_eq!(result.as_int(), Some(21));
    }

    #[test]
    #[cfg(feature = "ffi")]
    fn test_call_native_value_roundtrip() {
        let func = make_func(
            identity_value as *const c_void,
            Signature::new(vec![CType::Value], CType::Value),
        );
        let original = Value::int(42);
        let result = unsafe { call_native(&func, &[original]).unwrap() };
        assert_eq!(result.as_int(), Some(42));
    }

    #[test]
    #[cfg(feature = "ffi")]
    fn test_call_native_cstr_return_cleanup() {
        let func = make_func(
            make_greeting as *const c_void,
            Signature::new(vec![], CType::CStr),
        );
        let result = unsafe { call_native(&func, &[]).unwrap() };
        let ptr = result.as_ptr().unwrap() as *const c_char;
        // SAFETY: pointer came from make_greeting's CString::into_raw.
        let s = unsafe { std::ffi::CStr::from_ptr(ptr).to_str().unwrap() };
        assert_eq!(s, "hello ffi");
        // SAFETY: pointer came from CString::into_raw in make_greeting.
        unsafe { free_cstr_value(result) };
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
        use super::super::native::NativeLibrary;
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
