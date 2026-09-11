//! Stable C API for embedding the Nulang runtime.
//!
//! This module exposes a minimal, ABI-stable boundary so that C (or any other
//! language that can call C) can create a runtime, compile Nulang source,
//! execute it, and read the results.
//!
//! All public functions are `#[no_mangle] extern "C"`. The `NulangRuntime`
//! and `NulangValue` types are `#[repr(C)]` and can be passed by pointer or
//! by value across the FFI boundary.

use std::collections::HashMap;
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
    /// Unique compiled modules. Repeated source is stored only once.
    modules: Vec<crate::bytecode::CodeModule>,
    /// Public C handle -> unique module index. This preserves fresh-handle
    /// semantics without deep-cloning a CodeModule for each repeated compile.
    module_handles: Vec<usize>,
    /// Source-hash -> unique compiled module index.
    compile_cache: HashMap<[u8; 32], usize>,
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
            module_handles: Vec::new(),
            compile_cache: HashMap::new(),
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

    fn fresh_handle_for(&mut self, module_index: usize) -> usize {
        let handle = self.module_handles.len();
        self.module_handles.push(module_index);
        handle
    }

    fn compile(&mut self, source: &str) -> Option<usize> {
        self.clear_error();

        let source_hash = *blake3::hash(source.as_bytes()).as_bytes();
        if let Some(&module_index) = self.compile_cache.get(&source_hash) {
            if self.modules.get(module_index).is_some() {
                return Some(self.fresh_handle_for(module_index));
            }
            // The cache is internal and modules are append-only, so a missing
            // module index should be impossible. Fall through and repair the
            // entry by recompiling rather than failing the FFI call.
            self.compile_cache.remove(&source_hash);
        }

        match compile_source(source) {
            Ok(module) => {
                let module_index = self.modules.len();
                self.modules.push(module);
                self.compile_cache.insert(source_hash, module_index);
                Some(self.fresh_handle_for(module_index))
            }
            Err(e) => {
                self.set_error(e);
                None
            }
        }
    }

    fn run(&mut self, module_handle: usize) -> Option<Value> {
        self.clear_error();
        let module_index = *self.module_handles.get(module_handle)?;
        let module = self.modules.get(module_index)?.clone();
        let mut vm = VM::new();
        vm.load_module(module);
        match vm.run() {
            Ok(value) => Some(value),
            Err(e) => {
                self.set_error(e);
                None
            }
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
        let text = value.to_string_repr();
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
    fn test_identical_source_reuses_compiled_module_with_fresh_handle() {
        let rt = nulang_runtime_new();
        assert!(!rt.is_null());

        let source = CString::new("40 + 2").unwrap();
        let first = unsafe { nulang_compile(rt, source.as_ptr()) };
        let second = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(first >= 0 && second >= 0);
        assert_ne!(
            first, second,
            "each compile call must retain fresh-handle semantics"
        );

        // SAFETY: rt is valid for the duration of this test.
        let runtime = unsafe { &*rt };
        assert_eq!(runtime.compile_cache.len(), 1);
        assert_eq!(runtime.modules.len(), 1);
        assert_eq!(runtime.module_handles.len(), 2);
        assert_eq!(
            runtime.module_handles[first as usize],
            runtime.module_handles[second as usize]
        );

        let first_value = unsafe { nulang_run(rt, first) };
        let second_value = unsafe { nulang_run(rt, second) };
        assert_eq!(nulang_value_int(first_value), 42);
        assert_eq!(nulang_value_int(second_value), 42);

        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_different_source_creates_distinct_compile_cache_entries() {
        let rt = nulang_runtime_new();
        let first_source = CString::new("1 + 1").unwrap();
        let second_source = CString::new("2 + 2").unwrap();

        let first = unsafe { nulang_compile(rt, first_source.as_ptr()) };
        let second = unsafe { nulang_compile(rt, second_source.as_ptr()) };
        assert!(first >= 0 && second >= 0);

        let runtime = unsafe { &*rt };
        assert_eq!(runtime.compile_cache.len(), 2);
        assert_eq!(runtime.modules.len(), 2);
        assert_ne!(
            runtime.module_handles[first as usize],
            runtime.module_handles[second as usize]
        );

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
}
