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
use crate::value_layout::{TAG_MASK, TAG_PTR};
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
    /// Holds CStrings returned by the value-to-string helpers.
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

    /// Resolve a public C module handle to the deduplicated module storage index.
    ///
    /// Public handles intentionally remain fresh for every compile call, while
    /// `modules` stores identical source only once. Every operation that touches
    /// a compiled module must cross this indirection boundary.
    fn module_index_for_handle(&self, module_handle: usize) -> Option<usize> {
        self.module_handles.get(module_handle).copied()
    }

    fn compile(&mut self, source: &str) -> Option<usize> {
        self.clear_error();

        let source_hash = *blake3::hash(source.as_bytes()).as_bytes();
        if let Some(&module_index) = self.compile_cache.get(&source_hash) {
            if self.modules.get(module_index).is_some() {
                return Some(self.fresh_handle_for(module_index));
            }
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
        let module_index = self.module_index_for_handle(module_handle)?;
        let module = self.modules.get(module_index)?.clone();
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
        let module_index = self.module_index_for_handle(module_handle)?;
        let module = self.modules.get(module_index)?.clone();
        let offset = module.function_offset_by_name(name)?;
        let mut vm = VM::new();
        vm.load_module(module);
        let mut arg_values = Vec::with_capacity(args.len());
        for &arg in args {
            match value_from_c(arg) {
                Ok(value) => arg_values.push(value),
                Err(message) => {
                    self.last_error = Some(message.to_string());
                    return None;
                }
            }
        }
        match vm.call_function(0, offset, &arg_values) {
            Ok(value) => Some(self.stabilize_string_value(value, &vm, module_handle)),
            Err(e) => {
                self.set_error(e);
                None
            }
        }
    }

    fn add_module_string(&mut self, module_handle: usize, s: &str) -> Option<Value> {
        let module_index = self.module_index_for_handle(module_handle)?;
        let module = self.modules.get_mut(module_index)?;
        let idx = module.add_string_constant(s);
        Some(Value::string(idx as u32))
    }

    fn stabilize_string_value(&mut self, value: Value, vm: &VM, module_handle: usize) -> Value {
        if value.as_string_id().is_some() {
            return value;
        }
        if let Some(bytes) = vm.string_bytes(value) {
            if let Some(module_index) = self.module_index_for_handle(module_handle) {
                if let Some(module) = self.modules.get_mut(module_index) {
                    let id = module
                        .add_string_constant(String::from_utf8_lossy(&bytes).into_owned())
                        as u32;
                    return Value::string(id);
                }
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
                    CString::new("<invalid error message>").unwrap_or(CString::new("").unwrap())
                });
                let ptr = cstr.as_ptr();
                self.error_cstring = Some(cstr);
                ptr
            }
            None => std::ptr::null(),
        }
    }

    fn cache_cstr(&mut self, text: String) -> *const c_char {
        let cstr = CString::new(text).unwrap_or_else(|_| CString::new("").unwrap());
        let ptr = cstr.as_ptr();
        self.string_cache.push(cstr);
        ptr
    }

    fn value_to_cached_cstr(&mut self, value: Value) -> Result<*const c_char, String> {
        if value.as_string_id().is_some() {
            return Err(
                "interned string value requires module provenance; use nulang_module_value_to_string"
                    .to_string(),
            );
        }
        Ok(self.cache_cstr(value.to_string_repr()))
    }

    fn module_value_to_cached_cstr(
        &mut self,
        module_handle: usize,
        value: Value,
    ) -> Result<*const c_char, String> {
        let module_index = self
            .module_index_for_handle(module_handle)
            .ok_or_else(|| "invalid module handle".to_string())?;

        let text = if let Some(id) = value.as_string_id() {
            self.modules
                .get(module_index)
                .and_then(|module| module.constants.get(id as usize))
                .and_then(|constant| match constant {
                    crate::bytecode::Constant::String(s) => Some(s.clone()),
                    _ => None,
                })
                .ok_or_else(|| {
                    format!(
                        "string id {} does not belong to module handle {}",
                        id, module_handle
                    )
                })?
        } else {
            value.to_string_repr()
        };

        Ok(self.cache_cstr(text))
    }
}

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

const INVALID_C_VALUE: &str =
    "C ABI rejected a pointer-tagged NulangValue: host heap pointers cannot be supplied by C";

fn value_from_c(value: NulangValue) -> Result<Value, &'static str> {
    if (value.raw & TAG_MASK) == TAG_PTR {
        return Err(INVALID_C_VALUE);
    }
    Value::try_from_untrusted_bits(value.raw)
}

#[no_mangle]
pub extern "C" fn nulang_runtime_new() -> *mut NulangRuntime {
    Box::into_raw(Box::new(NulangRuntime::new()))
}

#[no_mangle]
pub unsafe extern "C" fn nulang_runtime_free(runtime: *mut NulangRuntime) {
    if !runtime.is_null() {
        unsafe {
            let _ = Box::from_raw(runtime);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn nulang_compile(runtime: *mut NulangRuntime, source: *const c_char) -> i64 {
    if runtime.is_null() || source.is_null() {
        return -1;
    }
    let source_str = unsafe {
        match CStr::from_ptr(source).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };
    let rt = unsafe { &mut *runtime };
    match rt.compile(source_str) {
        Some(handle) => handle as i64,
        None => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn nulang_run(
    runtime: *mut NulangRuntime,
    module_handle: i64,
) -> NulangValue {
    if runtime.is_null() || module_handle < 0 {
        return Value::nil().into();
    }
    let rt = unsafe { &mut *runtime };
    match rt.run(module_handle as usize) {
        Some(value) => value.into(),
        None => Value::nil().into(),
    }
}

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
        unsafe { std::slice::from_raw_parts(args, arg_count) }
    };
    match rt.call_function(module_handle as usize, name_str, args_slice) {
        Some(value) => value.into(),
        None => Value::nil().into(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn nulang_clear_error(runtime: *mut NulangRuntime) {
    if !runtime.is_null() {
        let rt = unsafe { &mut *runtime };
        rt.clear_error();
    }
}

#[no_mangle]
pub unsafe extern "C" fn nulang_last_error(runtime: *mut NulangRuntime) -> *const c_char {
    if runtime.is_null() {
        return std::ptr::null();
    }
    let rt = unsafe { &mut *runtime };
    rt.last_error_ptr()
}

#[no_mangle]
pub extern "C" fn nulang_value_int(value: NulangValue) -> i64 {
    value_from_c(value)
        .ok()
        .and_then(|value| value.as_int())
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn nulang_value_float(value: NulangValue) -> f64 {
    value_from_c(value)
        .ok()
        .and_then(|value| value.as_float())
        .unwrap_or(0.0)
}

#[no_mangle]
pub extern "C" fn nulang_value_bool(value: NulangValue) -> bool {
    value_from_c(value)
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn nulang_value_is_nil(value: NulangValue) -> bool {
    value_from_c(value)
        .map(|value| value.is_nil())
        .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn nulang_value_is_unit(value: NulangValue) -> bool {
    value_from_c(value)
        .map(|value| value.is_unit())
        .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn nulang_value_int_new(value: i64) -> NulangValue {
    Value::int(value).into()
}

#[no_mangle]
pub extern "C" fn nulang_value_float_new(value: f64) -> NulangValue {
    Value::float(value).into()
}

#[no_mangle]
pub extern "C" fn nulang_value_bool_new(value: bool) -> NulangValue {
    Value::bool(value).into()
}

#[no_mangle]
pub extern "C" fn nulang_value_nil() -> NulangValue {
    Value::nil().into()
}

#[no_mangle]
pub extern "C" fn nulang_value_unit() -> NulangValue {
    Value::unit().into()
}

#[no_mangle]
pub unsafe extern "C" fn nulang_module_string(
    runtime: *mut NulangRuntime,
    module_handle: i64,
    s: *const c_char,
) -> NulangValue {
    if runtime.is_null() || module_handle < 0 || s.is_null() {
        return Value::nil().into();
    }
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

#[no_mangle]
pub unsafe extern "C" fn nulang_free_string(
    runtime: *mut NulangRuntime,
    ptr: *const c_char,
) -> bool {
    if runtime.is_null() || ptr.is_null() {
        return false;
    }
    let rt = unsafe { &mut *runtime };
    rt.free_cached_string(ptr)
}

#[no_mangle]
pub unsafe extern "C" fn nulang_value_to_string(
    runtime: *mut NulangRuntime,
    value: NulangValue,
) -> *const c_char {
    if runtime.is_null() {
        return std::ptr::null();
    }
    let rt = unsafe { &mut *runtime };
    match value_from_c(value) {
        Ok(value) => match rt.value_to_cached_cstr(value) {
            Ok(ptr) => ptr,
            Err(message) => {
                rt.last_error = Some(message);
                std::ptr::null()
            }
        },
        Err(message) => {
            rt.last_error = Some(message.to_string());
            std::ptr::null()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn nulang_module_value_to_string(
    runtime: *mut NulangRuntime,
    module_handle: i64,
    value: NulangValue,
) -> *const c_char {
    if runtime.is_null() || module_handle < 0 {
        return std::ptr::null();
    }
    let rt = unsafe { &mut *runtime };
    match value_from_c(value) {
        Ok(value) => match rt.module_value_to_cached_cstr(module_handle as usize, value) {
            Ok(ptr) => ptr,
            Err(message) => {
                rt.last_error = Some(message);
                std::ptr::null()
            }
        },
        Err(message) => {
            rt.last_error = Some(message.to_string());
            std::ptr::null()
        }
    }
}

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
        unsafe { std::slice::from_raw_parts(params, param_count) }
    };
    let signature = super::marshal::Signature::new(params_slice.to_vec(), ret);

    unsafe {
        match super::native::register_native_function(name_str, ptr, signature) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn test_c_api_compile_and_run() {
        let rt = nulang_runtime_new();
        let source = CString::new("1 + 2").unwrap();
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(handle >= 0);
        let value = unsafe { nulang_run(rt, handle) };
        assert_eq!(nulang_value_int(value), 3);
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_identical_source_reuses_compiled_module_with_fresh_handle() {
        let rt = nulang_runtime_new();
        let source = CString::new("40 + 2").unwrap();
        let first = unsafe { nulang_compile(rt, source.as_ptr()) };
        let second = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(first >= 0 && second >= 0 && first != second);
        let runtime = unsafe { &*rt };
        assert_eq!(runtime.compile_cache.len(), 1);
        assert_eq!(runtime.modules.len(), 1);
        assert_eq!(runtime.module_handles.len(), 2);
        assert_eq!(
            runtime.module_handles[first as usize],
            runtime.module_handles[second as usize]
        );
        assert_eq!(nulang_value_int(unsafe { nulang_run(rt, first) }), 42);
        assert_eq!(nulang_value_int(unsafe { nulang_run(rt, second) }), 42);
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_fresh_cached_handle_supports_call_function() {
        let rt = nulang_runtime_new();
        let source = CString::new("fn add(a: Int, b: Int) -> Int { a + b } add(0, 0)").unwrap();
        let first = unsafe { nulang_compile(rt, source.as_ptr()) };
        let second = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(first >= 0 && second >= 0 && first != second);
        let args = [nulang_value_int_new(20), nulang_value_int_new(22)];
        let name = CString::new("add").unwrap();
        let result = unsafe {
            nulang_call_function(rt, second, name.as_ptr(), args.as_ptr(), args.len())
        };
        assert_eq!(nulang_value_int(result), 42);
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_fresh_cached_handle_supports_module_strings_and_string_returns() {
        let rt = nulang_runtime_new();
        let source = CString::new(
            "fn greet(name: String) -> String { perform String.concat(\"hello \", name) } greet(\"world\")",
        )
        .unwrap();
        let first = unsafe { nulang_compile(rt, source.as_ptr()) };
        let second = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(first >= 0 && second >= 0 && first != second);
        let name = CString::new("world").unwrap();
        let name_val = unsafe { nulang_module_string(rt, second, name.as_ptr()) };
        let args = [name_val];
        let func_name = CString::new("greet").unwrap();
        let result = unsafe {
            nulang_call_function(rt, second, func_name.as_ptr(), args.as_ptr(), args.len())
        };
        let repr = unsafe { nulang_module_value_to_string(rt, second, result) };
        assert_eq!(unsafe { CStr::from_ptr(repr).to_str().unwrap() }, "hello world");
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
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_compile_error() {
        let rt = nulang_runtime_new();
        let source = CString::new("let x = in").unwrap();
        assert_eq!(unsafe { nulang_compile(rt, source.as_ptr()) }, -1);
        assert!(!unsafe { nulang_last_error(rt) }.is_null());
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_value_extractors() {
        assert_eq!(nulang_value_int(Value::int(42).into()), 42);
        assert!((nulang_value_float(Value::float(2.5).into()) - 2.5).abs() < f64::EPSILON);
        assert!(nulang_value_bool(Value::bool(true).into()));
        assert!(nulang_value_is_nil(Value::nil().into()));
        assert!(nulang_value_is_unit(Value::unit().into()));
    }

    #[test]
    fn test_c_api_rejects_forged_host_pointer_values() {
        let forged = NulangValue { raw: TAG_PTR | 0x1234 };
        assert_eq!(nulang_value_int(forged), 0);
        let rt = nulang_runtime_new();
        assert!(unsafe { nulang_value_to_string(rt, forged) }.is_null());
        let err = unsafe { nulang_last_error(rt) };
        assert!(unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .contains("pointer-tagged"));
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_call_rejects_forged_host_pointer_argument() {
        let rt = nulang_runtime_new();
        let source = CString::new("fn id(x: Int) -> Int { x } id(0)").unwrap();
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        let name = CString::new("id").unwrap();
        let forged = [NulangValue { raw: TAG_PTR | 0x1234 }];
        let result = unsafe {
            nulang_call_function(rt, handle, name.as_ptr(), forged.as_ptr(), forged.len())
        };
        assert!(nulang_value_is_nil(result));
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_value_to_string() {
        let rt = nulang_runtime_new();
        let ptr = unsafe { nulang_value_to_string(rt, Value::int(123).into()) };
        assert_eq!(unsafe { CStr::from_ptr(ptr).to_str().unwrap() }, "123");
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_value_constructors() {
        assert_eq!(nulang_value_int(nulang_value_int_new(-7)), -7);
        assert!((nulang_value_float(nulang_value_float_new(3.5)) - 3.5).abs() < f64::EPSILON);
        assert!(nulang_value_bool(nulang_value_bool_new(true)));
        assert!(nulang_value_is_nil(nulang_value_nil()));
        assert!(nulang_value_is_unit(nulang_value_unit()));
    }

    #[test]
    fn test_c_api_call_function() {
        let rt = nulang_runtime_new();
        let source = CString::new("fn add(a: Int, b: Int) -> Int { a + b } add(0, 0)").unwrap();
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        let args = [nulang_value_int_new(10), nulang_value_int_new(32)];
        let name = CString::new("add").unwrap();
        let result = unsafe {
            nulang_call_function(rt, handle, name.as_ptr(), args.as_ptr(), args.len())
        };
        assert_eq!(nulang_value_int(result), 42);
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_string_return_stabilized() {
        let rt = nulang_runtime_new();
        let source = CString::new(
            "fn greet(name: String) -> String { perform String.concat(\"hello \", name) } greet(\"world\")",
        )
        .unwrap();
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        let name = CString::new("world").unwrap();
        let name_val = unsafe { nulang_module_string(rt, handle, name.as_ptr()) };
        let args = [name_val];
        let func_name = CString::new("greet").unwrap();
        let result = unsafe {
            nulang_call_function(rt, handle, func_name.as_ptr(), args.as_ptr(), args.len())
        };
        let repr = unsafe { nulang_module_value_to_string(rt, handle, result) };
        assert_eq!(unsafe { CStr::from_ptr(repr).to_str().unwrap() }, "hello world");
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_module_aware_string_conversion_preserves_provenance() {
        let rt = nulang_runtime_new();
        let first = unsafe { nulang_compile(rt, CString::new("1").unwrap().as_ptr()) };
        let second = unsafe { nulang_compile(rt, CString::new("2").unwrap().as_ptr()) };
        let alpha = CString::new("alpha").unwrap();
        let beta = CString::new("beta").unwrap();
        let first_value = unsafe { nulang_module_string(rt, first, alpha.as_ptr()) };
        let second_value = unsafe { nulang_module_string(rt, second, beta.as_ptr()) };
        assert_eq!(
            value_from_c(first_value).unwrap().as_string_id(),
            value_from_c(second_value).unwrap().as_string_id()
        );
        let first_ptr = unsafe { nulang_module_value_to_string(rt, first, first_value) };
        let second_ptr = unsafe { nulang_module_value_to_string(rt, second, second_value) };
        assert_eq!(unsafe { CStr::from_ptr(first_ptr).to_str().unwrap() }, "alpha");
        assert_eq!(unsafe { CStr::from_ptr(second_ptr).to_str().unwrap() }, "beta");
        assert!(unsafe { nulang_value_to_string(rt, first_value) }.is_null());
        let err = unsafe { nulang_last_error(rt) };
        assert!(unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .contains("module provenance"));
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_module_string() {
        let rt = nulang_runtime_new();
        let source =
            CString::new("fn len(s: String) -> Int { perform String.length(s) } len(\"\")")
                .unwrap();
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        let s = CString::new("hello c api").unwrap();
        let sv = unsafe { nulang_module_string(rt, handle, s.as_ptr()) };
        let args = [sv];
        let name = CString::new("len").unwrap();
        let result = unsafe {
            nulang_call_function(rt, handle, name.as_ptr(), args.as_ptr(), args.len())
        };
        assert_eq!(nulang_value_int(result), 11);
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_free_string() {
        let rt = nulang_runtime_new();
        let ptr = unsafe { nulang_value_to_string(rt, Value::int(456).into()) };
        assert!(!ptr.is_null());
        assert!(unsafe { nulang_free_string(rt, ptr) });
        assert!(!unsafe { nulang_free_string(rt, std::ptr::null()) });
        unsafe { nulang_runtime_free(rt) };
    }
}
