//! Stable C API for embedding the Nulang runtime.
//!
//! This module exposes a minimal, ABI-stable boundary so that C (or any other
//! language that can call C) can create a runtime, compile Nulang source,
//! execute it, and read the results.
//!
//! All public functions are `#[no_mangle] extern "C"`. The `NulangRuntime`
//! and `NulangValue` types are `#[repr(C)]` and can be passed by pointer or
//! by value across the FFI boundary.

use std::ffi::{c_char, c_void, CStr, CString};

use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::typechecker::TypeChecker;
use crate::types::NuError;
use crate::vm::{Value, VM};

// ---------------------------------------------------------------------------
// Opaque runtime handle
// ---------------------------------------------------------------------------

/// An opaque runtime context that owns compiled modules and error state.
#[repr(C)]
pub struct NulangRuntime {
    modules: Vec<crate::bytecode::CodeModule>,
    last_error: Option<String>,
    /// Holds the CString backing `nulang_last_error`.
    error_cstring: Option<CString>,
    /// Holds CStrings returned by `nulang_value_to_string`.
    string_cache: Vec<CString>,
}

impl NulangRuntime {
    fn new() -> Self {
        NulangRuntime {
            modules: Vec::new(),
            last_error: None,
            error_cstring: None,
            string_cache: Vec::new(),
        }
    }

    fn set_error(&mut self, err: NuError) {
        self.last_error = Some(err.to_string());
    }

    fn clear_error(&mut self) {
        self.last_error = None;
        self.error_cstring = None;
    }

    fn compile(&mut self, source: &str) -> Option<usize> {
        self.clear_error();
        match compile_source(source) {
            Ok(module) => {
                let handle = self.modules.len();
                self.modules.push(module);
                Some(handle)
            }
            Err(e) => {
                self.set_error(e);
                None
            }
        }
    }

    fn run(&mut self, module_handle: usize) -> Option<Value> {
        self.clear_error();
        let module = self.modules.get(module_handle)?.clone();
        let mut vm = VM::new();
        vm.load_module(module);
        match vm.run() {
            Ok(value) => Some(self.stabilize_string_value(value, &vm, module_handle)),
            Err(e) => {
                self.set_error(e);
                None
            }
        }
    }

    fn call_function(
        &mut self,
        module_handle: usize,
        name: &str,
        args: &[NulangValue],
    ) -> Option<Value> {
        self.clear_error();
        let module = self.modules.get(module_handle)?.clone();
        let offset = module.function_offset_by_name(name)?;
        let mut vm = VM::new();
        vm.load_module(module);
        let arg_values: Vec<Value> = args.iter().map(|v| Value::from(*v)).collect();
        match vm.call_function(0, offset, &arg_values) {
            Ok(value) => Some(self.stabilize_string_value(value, &vm, module_handle)),
            Err(e) => {
                self.set_error(e);
                None
            }
        }
    }

    fn add_module_string(&mut self, module_handle: usize, s: &str) -> Option<Value> {
        let module = self.modules.get_mut(module_handle)?;
        let idx = module.add_string_constant(s);
        Some(Value::string(idx as u32))
    }

    /// Move a returned string value from the ephemeral VM heap into the
    /// runtime's module constant pool so that it remains valid after the VM is
    /// dropped. String-pool values are returned unchanged; non-string values are
    /// returned unchanged.
    fn stabilize_string_value(&mut self, value: Value, vm: &VM, module_handle: usize) -> Value {
        if value.as_string_id().is_some() {
            return value;
        }
        if let Some(bytes) = vm.string_bytes(value) {
            if let Some(module) = self.modules.get_mut(module_handle) {
                let id =
                    module.add_string_constant(String::from_utf8_lossy(&bytes).into_owned()) as u32;
                return Value::string(id);
            }
        }
        value
    }

    fn free_cached_string(&mut self, ptr: *const c_char) -> bool {
        let found = self.string_cache.iter().position(|c| c.as_ptr() == ptr);
        if let Some(idx) = found {
            self.string_cache.swap_remove(idx);
            true
        } else {
            false
        }
    }

    fn last_error_ptr(&mut self) -> *const c_char {
        match &self.last_error {
            Some(msg) => {
                let cstr = CString::new(msg.clone()).unwrap_or_else(|_| {
                    // The message should never contain interior nuls in practice.
                    CString::new("<invalid error message>").unwrap_or(CString::new("").unwrap())
                });
                let ptr = cstr.as_ptr();
                self.error_cstring = Some(cstr);
                ptr
            }
            None => std::ptr::null(),
        }
    }

    fn value_to_cached_cstr(&mut self, value: Value) -> *const c_char {
        let text = if let Some(id) = value.as_string_id() {
            let module_idx = self.modules.len().saturating_sub(1);
            self.modules
                .get(module_idx)
                .and_then(|m| m.constants.get(id as usize))
                .and_then(|c| match c {
                    crate::bytecode::Constant::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| value.to_string_repr())
        } else {
            value.to_string_repr()
        };
        let cstr = CString::new(text).unwrap_or_else(|_| CString::new("").unwrap());
        let ptr = cstr.as_ptr();
        self.string_cache.push(cstr);
        ptr
    }
}

// ---------------------------------------------------------------------------
// Compilation pipeline
// ---------------------------------------------------------------------------

fn compile_source(source: &str) -> Result<crate::bytecode::CodeModule, NuError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.lex()?;

    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module()?;

    let mut type_checker = TypeChecker::new();
    let _module_type = type_checker.check_module(&ast)?;

    let mut effect_checker = EffectChecker::new();
    effect_checker.check_module(&ast.decls)?;

    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_ctx = CapContext::new();
    for decl in crate::effect_checker::flatten_decls(&ast.decls) {
        match decl {
            crate::ast::Decl::Function { body, params, .. } => {
                let ctx = cap_ctx.with_params(params);
                cap_analyzer.infer_cap(&ctx, body)?;
            }
            crate::ast::Decl::Actor { behaviors, .. } => {
                for behavior in behaviors {
                    let ctx = cap_ctx.with_params(&behavior.params);
                    cap_analyzer.infer_cap(&ctx, &behavior.body)?;
                }
            }
            _ => {}
        }
    }

    let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir)?;
    let code_module = crate::mir_codegen::compile_mir(&mut mir, "main")?;
    Ok(code_module)
}

// ---------------------------------------------------------------------------
// C value type
// ---------------------------------------------------------------------------

/// A Nulang value exposed to C.
///
/// Internally this is just the raw NaN-boxed bits. Use the extractor
/// functions below to read primitive data out of it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NulangValue {
    raw: u64,
}

impl From<Value> for NulangValue {
    fn from(value: Value) -> Self {
        NulangValue {
            raw: value.to_bits(),
        }
    }
}

impl From<NulangValue> for Value {
    fn from(value: NulangValue) -> Self {
        Value::from_bits(value.raw)
    }
}

// ---------------------------------------------------------------------------
// C API functions
// ---------------------------------------------------------------------------

/// Create a new Nulang runtime.
#[no_mangle]
pub extern "C" fn nulang_runtime_new() -> *mut NulangRuntime {
    let runtime = Box::new(NulangRuntime::new());
    Box::into_raw(runtime)
}

/// Free a Nulang runtime created by `nulang_runtime_new`.
///
/// # Safety
/// `runtime` must be a pointer returned by `nulang_runtime_new` and must not
/// be used after this call.
#[no_mangle]
pub unsafe extern "C" fn nulang_runtime_free(runtime: *mut NulangRuntime) {
    if !runtime.is_null() {
        // SAFETY: caller guarantees the pointer came from `nulang_runtime_new`.
        unsafe {
            let _ = Box::from_raw(runtime);
        }
    }
}

/// Compile Nulang source code.
///
/// Returns a non-negative module handle on success, or -1 on error.
/// On error, the error message can be retrieved with `nulang_last_error`.
///
/// # Safety
/// `source` must be a valid, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn nulang_compile(runtime: *mut NulangRuntime, source: *const c_char) -> i64 {
    if runtime.is_null() || source.is_null() {
        return -1;
    }
    // SAFETY: caller guarantees `source` is a valid C string.
    let source_str = unsafe {
        match CStr::from_ptr(source).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };
    // SAFETY: runtime is non-null and valid.
    let rt = unsafe { &mut *runtime };
    match rt.compile(source_str) {
        Some(handle) => handle as i64,
        None => -1,
    }
}

/// Run a previously compiled module.
///
/// Returns the resulting value. If execution failed, the result is `nil` and
/// `nulang_last_error` will return the error message.
///
/// # Safety
/// `runtime` must be a valid pointer returned by `nulang_runtime_new`.
#[no_mangle]
pub unsafe extern "C" fn nulang_run(
    runtime: *mut NulangRuntime,
    module_handle: i64,
) -> NulangValue {
    if runtime.is_null() || module_handle < 0 {
        return Value::nil().into();
    }
    // SAFETY: runtime is non-null and valid.
    let rt = unsafe { &mut *runtime };
    match rt.run(module_handle as usize) {
        Some(value) => value.into(),
        None => Value::nil().into(),
    }
}

/// Call an exported Nulang function by name with the supplied arguments.
///
/// `args` points to an array of `NulangValue` of length `arg_count`. The
/// first argument goes into register r0, the second into r1, and so on,
/// matching the MIR calling convention. On success the function's return
/// value (register 0) is returned; on error the result is `nil` and
/// `nulang_last_error` contains the message.
///
/// # Safety
/// `runtime` must be valid, `name` a null-terminated UTF-8 string, and
/// `args` must point to `arg_count` valid values (or be NULL when zero).
#[no_mangle]
pub unsafe extern "C" fn nulang_call_function(
    runtime: *mut NulangRuntime,
    module_handle: i64,
    name: *const c_char,
    args: *const NulangValue,
    arg_count: usize,
) -> NulangValue {
    if runtime.is_null() || module_handle < 0 || name.is_null() {
        return Value::nil().into();
    }
    // SAFETY: runtime is valid; caller guarantees name is a valid C string.
    let rt = unsafe { &mut *runtime };
    let name_str = match unsafe { CStr::from_ptr(name).to_str() } {
        Ok(s) => s,
        Err(_) => return Value::nil().into(),
    };
    let args_slice = if arg_count == 0 {
        &[]
    } else if args.is_null() {
        return Value::nil().into();
    } else {
        // SAFETY: caller guarantees args points to arg_count valid values.
        unsafe { std::slice::from_raw_parts(args, arg_count) }
    };
    match rt.call_function(module_handle as usize, name_str, args_slice) {
        Some(value) => value.into(),
        None => Value::nil().into(),
    }
}

/// Clear the runtime's last error state.
///
/// # Safety
/// `runtime` must be a valid pointer returned by `nulang_runtime_new`.
#[no_mangle]
pub unsafe extern "C" fn nulang_clear_error(runtime: *mut NulangRuntime) {
    if !runtime.is_null() {
        // SAFETY: runtime is valid.
        let rt = unsafe { &mut *runtime };
        rt.clear_error();
    }
}

/// Return the last error message, or `NULL` if there is none.
///
/// The returned pointer is owned by the runtime and remains valid until the
/// next call that modifies the error state or until the runtime is freed.
///
/// # Safety
/// `runtime` must be a valid pointer returned by `nulang_runtime_new`.
#[no_mangle]
pub unsafe extern "C" fn nulang_last_error(runtime: *mut NulangRuntime) -> *const c_char {
    if runtime.is_null() {
        return std::ptr::null();
    }
    // SAFETY: runtime is non-null and valid.
    let rt = unsafe { &mut *runtime };
    rt.last_error_ptr()
}

/// Extract an integer from a Nulang value.
///
/// Returns 0 if the value is not an integer.
#[no_mangle]
pub extern "C" fn nulang_value_int(value: NulangValue) -> i64 {
    Value::from(value).as_int().unwrap_or(0)
}

/// Extract a float from a Nulang value.
///
/// Returns 0.0 if the value is not a float.
#[no_mangle]
pub extern "C" fn nulang_value_float(value: NulangValue) -> f64 {
    Value::from(value).as_float().unwrap_or(0.0)
}

/// Extract a boolean from a Nulang value.
///
/// Returns `false` if the value is not a boolean.
#[no_mangle]
pub extern "C" fn nulang_value_bool(value: NulangValue) -> bool {
    Value::from(value).as_bool().unwrap_or(false)
}

/// Check whether a value is `nil`.
#[no_mangle]
pub extern "C" fn nulang_value_is_nil(value: NulangValue) -> bool {
    Value::from(value).is_nil()
}

/// Check whether a value is the unit value `()`.
#[no_mangle]
pub extern "C" fn nulang_value_is_unit(value: NulangValue) -> bool {
    Value::from(value).is_unit()
}

/// Create an integer Nulang value.
#[no_mangle]
pub extern "C" fn nulang_value_int_new(value: i64) -> NulangValue {
    Value::int(value).into()
}

/// Create a floating-point Nulang value.
#[no_mangle]
pub extern "C" fn nulang_value_float_new(value: f64) -> NulangValue {
    Value::float(value).into()
}

/// Create a boolean Nulang value.
#[no_mangle]
pub extern "C" fn nulang_value_bool_new(value: bool) -> NulangValue {
    Value::bool(value).into()
}

/// Create the nil Nulang value.
#[no_mangle]
pub extern "C" fn nulang_value_nil() -> NulangValue {
    Value::nil().into()
}

/// Create the unit Nulang value `()`.
#[no_mangle]
pub extern "C" fn nulang_value_unit() -> NulangValue {
    Value::unit().into()
}

/// Create a Nulang string value by interning `s` into a compiled module's
/// constant pool.
///
/// Returns `nil` if the runtime or handle is invalid, if `s` is not valid
/// UTF-8, or if the string cannot be interned.
///
/// # Safety
/// `runtime` must be valid and `s` a null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn nulang_module_string(
    runtime: *mut NulangRuntime,
    module_handle: i64,
    s: *const c_char,
) -> NulangValue {
    if runtime.is_null() || module_handle < 0 || s.is_null() {
        return Value::nil().into();
    }
    // SAFETY: runtime is valid; caller guarantees s is a valid C string.
    let rt = unsafe { &mut *runtime };
    let s_str = match unsafe { CStr::from_ptr(s).to_str() } {
        Ok(s) => s,
        Err(_) => return Value::nil().into(),
    };
    match rt.add_module_string(module_handle as usize, s_str) {
        Some(value) => value.into(),
        None => Value::nil().into(),
    }
}

/// Free a C string previously returned by `nulang_value_to_string`.
///
/// Returns `true` if the pointer was recognized and freed, `false` otherwise.
///
/// # Safety
/// `runtime` must be valid and `ptr` must be a pointer previously returned by
/// `nulang_value_to_string` and not already freed.
#[no_mangle]
pub unsafe extern "C" fn nulang_free_string(
    runtime: *mut NulangRuntime,
    ptr: *const c_char,
) -> bool {
    if runtime.is_null() || ptr.is_null() {
        return false;
    }
    // SAFETY: runtime is valid.
    let rt = unsafe { &mut *runtime };
    rt.free_cached_string(ptr)
}

/// Return a C string representation of a Nulang value.
///
/// The returned pointer is owned by the runtime and remains valid until the
/// runtime is freed. The caller must not free it.
///
/// # Safety
/// `runtime` must be a valid pointer returned by `nulang_runtime_new`.
#[no_mangle]
pub unsafe extern "C" fn nulang_value_to_string(
    runtime: *mut NulangRuntime,
    value: NulangValue,
) -> *const c_char {
    if runtime.is_null() {
        return std::ptr::null();
    }
    // SAFETY: runtime is non-null and valid.
    let rt = unsafe { &mut *runtime };
    rt.value_to_cached_cstr(Value::from(value))
}

/// Register a native C function so it can be called from Nulang.
///
/// The function pointer must match the supplied parameter and return types.
/// Use `"__nulang_registered__"` as the sentinel library name in the Nulang
/// `extern` block when no dynamic library is required.
///
/// `params` is a pointer to an array of `CType` values; `param_count` is the
/// array length. `ret` is the return type. The pointer and array are only
/// borrowed for the duration of the call.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
/// `name` must be a valid null-terminated UTF-8 string. `ptr` must point to a
/// valid function whose C ABI matches the given types. If `param_count` is
/// non-zero, `params` must point to at least `param_count` valid `CType`
/// values.
#[no_mangle]
pub unsafe extern "C" fn nulang_register_native_function(
    name: *const c_char,
    ptr: *const c_void,
    params: *const super::marshal::CType,
    param_count: usize,
    ret: super::marshal::CType,
) -> i32 {
    if name.is_null() || ptr.is_null() {
        return -1;
    }
    // SAFETY: caller guarantees `name` is a valid C string.
    let name_str = unsafe {
        match CStr::from_ptr(name).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };

    let params_slice = if param_count == 0 {
        &[]
    } else if params.is_null() {
        return -1;
    } else {
        // SAFETY: caller guarantees `params` points to `param_count` valid values.
        unsafe { std::slice::from_raw_parts(params, param_count) }
    };
    let signature = super::marshal::Signature::new(params_slice.to_vec(), ret);

    // SAFETY: caller guarantees `ptr` matches the signature.
    unsafe {
        match super::native::register_native_function(name_str, ptr, signature) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn test_c_api_compile_and_run() {
        let rt = nulang_runtime_new();
        assert!(!rt.is_null());

        let source = CString::new("1 + 2").unwrap();
        // SAFETY: rt is valid and source is a valid C string.
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(handle >= 0, "compile failed");

        // SAFETY: rt is valid and handle is valid.
        let value = unsafe { nulang_run(rt, handle) };
        assert_eq!(nulang_value_int(value), 3);

        // SAFETY: rt is valid.
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_compile_error() {
        let rt = nulang_runtime_new();
        let source = CString::new("let x = in").unwrap();
        // SAFETY: rt is valid and source is a valid C string.
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert_eq!(handle, -1);

        // SAFETY: rt is valid.
        let err = unsafe { nulang_last_error(rt) };
        assert!(!err.is_null());

        // SAFETY: rt is valid.
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_value_extractors() {
        let int_val: NulangValue = Value::int(42).into();
        assert_eq!(nulang_value_int(int_val), 42);

        let float_val: NulangValue = Value::float(2.5).into();
        assert!((nulang_value_float(float_val) - 2.5).abs() < f64::EPSILON);

        let bool_val: NulangValue = Value::bool(true).into();
        assert!(nulang_value_bool(bool_val));

        assert!(nulang_value_is_nil(Value::nil().into()));
        assert!(nulang_value_is_unit(Value::unit().into()));
    }

    #[test]
    fn test_c_api_value_to_string() {
        let rt = nulang_runtime_new();
        let value: NulangValue = Value::int(123).into();
        // SAFETY: rt is valid.
        let ptr = unsafe { nulang_value_to_string(rt, value) };
        assert!(!ptr.is_null());
        // SAFETY: ptr points to a valid CString owned by the runtime.
        let s = unsafe { CStr::from_ptr(ptr).to_str().unwrap() };
        assert_eq!(s, "123");

        // SAFETY: rt is valid.
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_value_constructors() {
        assert_eq!(nulang_value_int(nulang_value_int_new(-7)), -7);
        let f = nulang_value_float(nulang_value_float_new(3.5));
        assert!((f - 3.5).abs() < f64::EPSILON);
        assert!(nulang_value_bool(nulang_value_bool_new(true)));
        assert!(nulang_value_is_nil(nulang_value_nil()));
        assert!(nulang_value_is_unit(nulang_value_unit()));
    }

    #[test]
    fn test_c_api_call_function() {
        let rt = nulang_runtime_new();
        let source = CString::new("fn add(a: Int, b: Int) -> Int { a + b } add(0, 0)").unwrap();
        // SAFETY: rt is valid and source is a valid C string.
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(handle >= 0, "compile failed");

        let args = [nulang_value_int_new(10), nulang_value_int_new(32)];
        // SAFETY: rt and handle valid, name is a valid C string.
        let name = CString::new("add").unwrap();
        let result =
            unsafe { nulang_call_function(rt, handle, name.as_ptr(), args.as_ptr(), args.len()) };
        assert_eq!(nulang_value_int(result), 42);

        // SAFETY: rt is valid.
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_string_return_stabilized() {
        let rt = nulang_runtime_new();
        let source = CString::new(
            "fn greet(name: String) -> String { perform String.concat(\"hello \", name) } greet(\"world\")",
        )
        .unwrap();
        // SAFETY: rt is valid and source is a valid C string.
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(handle >= 0, "compile failed");

        let name = CString::new("world").unwrap();
        // SAFETY: rt, handle, and name are valid.
        let name_val = unsafe { nulang_module_string(rt, handle, name.as_ptr()) };
        assert!(!nulang_value_is_nil(name_val));

        let args = [name_val];
        let func_name = CString::new("greet").unwrap();
        // SAFETY: rt and handle valid, name is a valid C string.
        let result = unsafe {
            nulang_call_function(rt, handle, func_name.as_ptr(), args.as_ptr(), args.len())
        };

        // SAFETY: rt is valid.
        let repr = unsafe { nulang_value_to_string(rt, result) };
        assert!(!repr.is_null());
        // SAFETY: repr points to a valid CString owned by the runtime.
        let s = unsafe { CStr::from_ptr(repr).to_str().unwrap() };
        assert_eq!(s, "hello world");

        // SAFETY: rt is valid.
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_module_string() {
        let rt = nulang_runtime_new();
        let source =
            CString::new("fn len(s: String) -> Int { perform String.length(s) } len(\"\")")
                .unwrap();
        // SAFETY: rt is valid and source is a valid C string.
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(handle >= 0, "compile failed");

        let s = CString::new("hello c api").unwrap();
        // SAFETY: rt, handle, and s are valid.
        let sv = unsafe { nulang_module_string(rt, handle, s.as_ptr()) };
        assert!(!nulang_value_is_nil(sv));

        let args = [sv];
        let name = CString::new("len").unwrap();
        // SAFETY: valid inputs.
        let result =
            unsafe { nulang_call_function(rt, handle, name.as_ptr(), args.as_ptr(), args.len()) };
        assert_eq!(nulang_value_int(result), 11);

        // SAFETY: rt is valid.
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_free_string() {
        let rt = nulang_runtime_new();
        let value: NulangValue = Value::int(456).into();
        // SAFETY: rt is valid.
        let ptr = unsafe { nulang_value_to_string(rt, value) };
        assert!(!ptr.is_null());
        // SAFETY: ptr was returned by nulang_value_to_string.
        assert!(unsafe { nulang_free_string(rt, ptr) });
        // SAFETY: null pointer is always safe to pass.
        assert!(!unsafe { nulang_free_string(rt, std::ptr::null()) });
        // SAFETY: rt is valid.
        unsafe { nulang_runtime_free(rt) };
    }
}
