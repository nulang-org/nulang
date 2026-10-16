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
    /// Native callbacks and loaded libraries belong to this embedding instance.
    ffi_registry: std::sync::Arc<std::sync::Mutex<super::native::FfiRegistry>>,
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
            ffi_registry: std::sync::Arc::new(std::sync::Mutex::new(
                super::native::FfiRegistry::new(),
            )),
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

    /// Resolve an externally visible C module handle to the unique compiled
    /// module it references. Public handles intentionally have fresh identity
    /// even when compilation reuses a cached module, so they must never be
    /// used directly as indices into `modules`.
    fn module_index_for_handle(&self, module_handle: usize) -> Option<usize> {
        let module_index = *self.module_handles.get(module_handle)?;
        self.modules.get(module_index)?;
        Some(module_index)
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
        let module_index = self.module_index_for_handle(module_handle)?;
        let module = self.modules.get(module_index)?.clone();
        let mut vm = VM::new();
        vm.set_ffi_registry(self.ffi_registry.clone());
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
        vm.set_ffi_registry(self.ffi_registry.clone());
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

    /// Move a returned string value from the ephemeral VM heap into the
    /// runtime's module constant pool so that it remains valid after the VM is
    /// dropped. String-pool values are returned unchanged; non-string values are
    /// returned unchanged.
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

    /// Resolve a string using its owning module. Legacy unqualified lookup is
    /// permitted only when every compiled module maps the id to identical text.
    /// A raw NaN-boxed string id is not sufficient to identify its provenance.
    fn resolve_module_string(
        &self,
        string_id: usize,
        module_index: Option<usize>,
    ) -> Result<String, String> {
        fn lookup(module: &crate::bytecode::CodeModule, string_id: usize) -> Option<&str> {
            module
                .constants
                .get(string_id)
                .and_then(|constant| match constant {
                    crate::bytecode::Constant::String(text) => Some(text.as_str()),
                    _ => None,
                })
        }

        if let Some(module_index) = module_index {
            return self
                .modules
                .get(module_index)
                .and_then(|module| lookup(module, string_id))
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!("string id {string_id} is not present in the selected module")
                });
        }

        let mut resolved: Option<&str> = None;
        for module in &self.modules {
            let current = lookup(module, string_id).ok_or_else(|| {
                "ambiguous string value across modules; use nulang_value_to_string_in_module"
                    .to_string()
            })?;
            if let Some(previous) = resolved {
                if previous != current {
                    return Err(
                        "ambiguous string value across modules; use nulang_value_to_string_in_module"
                            .to_string(),
                    );
                }
            } else {
                resolved = Some(current);
            }
        }
        resolved.map(str::to_owned).ok_or_else(|| {
            "string value has no compiled module; use nulang_value_to_string_in_module".to_string()
        })
    }

    fn value_to_cached_cstr(
        &mut self,
        value: Value,
        module_index: Option<usize>,
    ) -> Result<*const c_char, String> {
        let text = if let Some(id) = value.as_string_id() {
            self.resolve_module_string(id as usize, module_index)?
        } else {
            value.to_string_repr()
        };
        let cstr = CString::new(text).map_err(|_| {
            "Nulang string cannot be represented as a C string (interior NUL)".to_string()
        })?;
        let ptr = cstr.as_ptr();
        self.string_cache.push(cstr);
        Ok(ptr)
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

const INVALID_C_VALUE: &str =
    "C ABI rejected a pointer-tagged NulangValue: host heap pointers cannot be supplied by C";

/// Convert an ABI value into a host `Value` only when it cannot forge a host
/// heap pointer. C callers may construct arbitrary `NulangValue` bits even
/// though `raw` is private to Rust, so the old blanket `From<NulangValue>`
/// implementation was not a trustworthy provenance boundary.
fn value_from_c(value: NulangValue) -> Result<Value, &'static str> {
    if (value.raw & TAG_MASK) == TAG_PTR {
        return Err(INVALID_C_VALUE);
    }
    Value::try_from_untrusted_bits(value.raw)
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
/// Pointer-tagged values are not accepted from C because their payload cannot
/// establish host-heap provenance.
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
        // SAFETY: caller guarantees args points to arg_count initialized ABI values.
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
/// Returns 0 if the value is not an integer or if C supplied a forbidden
/// host-pointer-tagged value.
#[no_mangle]
pub extern "C" fn nulang_value_int(value: NulangValue) -> i64 {
    value_from_c(value)
        .ok()
        .and_then(|value| value.as_int())
        .unwrap_or(0)
}

/// Extract a float from a Nulang value.
///
/// Returns 0.0 if the value is not a float or is rejected at the C boundary.
#[no_mangle]
pub extern "C" fn nulang_value_float(value: NulangValue) -> f64 {
    value_from_c(value)
        .ok()
        .and_then(|value| value.as_float())
        .unwrap_or(0.0)
}

/// Extract a boolean from a Nulang value.
///
/// Returns `false` if the value is not a boolean or is rejected at the C boundary.
#[no_mangle]
pub extern "C" fn nulang_value_bool(value: NulangValue) -> bool {
    value_from_c(value)
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Check whether a value is `nil`.
#[no_mangle]
pub extern "C" fn nulang_value_is_nil(value: NulangValue) -> bool {
    value_from_c(value)
        .map(|value| value.is_nil())
        .unwrap_or(false)
}

/// Check whether a value is the unit value `()`.
#[no_mangle]
pub extern "C" fn nulang_value_is_unit(value: NulangValue) -> bool {
    value_from_c(value)
        .map(|value| value.is_unit())
        .unwrap_or(false)
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
/// Pointer-tagged raw values supplied by C are rejected because a numeric
/// payload cannot establish that it points into a live Nulang host heap.
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
    rt.clear_error();
    match value_from_c(value) {
        Ok(value) => match rt.value_to_cached_cstr(value, None) {
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

/// Convert a string value using the compiled module that owns its string pool.
///
/// Unlike the legacy converter, this function never guesses a string's module
/// from raw tagged bits. The caller must pass the handle of the module from
/// which the value originated. Non-string primitive values do not use a pool.
///
/// # Safety
/// `runtime` must be a live runtime pointer. `module_handle` must refer
/// to a module compiled by that runtime.
#[no_mangle]
pub unsafe extern "C" fn nulang_value_to_string_in_module(
    runtime: *mut NulangRuntime,
    module_handle: i64,
    value: NulangValue,
) -> *const c_char {
    if runtime.is_null() {
        return std::ptr::null();
    }
    // SAFETY: caller guarantees a live, exclusively accessed runtime.
    let rt = unsafe { &mut *runtime };
    rt.clear_error();
    let module_index = if module_handle >= 0 {
        rt.module_index_for_handle(module_handle as usize)
    } else {
        None
    };
    let Some(module_index) = module_index else {
        rt.last_error = Some("invalid compiled module handle".to_string());
        return std::ptr::null();
    };
    match value_from_c(value) {
        Ok(value) => match rt.value_to_cached_cstr(value, Some(module_index)) {
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

/// Register a native C callback scoped to a single embedded runtime.
///
/// Names registered here cannot be resolved by other embedded runtimes.
/// The callback pointer must remain valid until the owning runtime is freed.
/// All runtime, string and signature pointers must be valid for the call.
/// Returns 0 on success, -1 on error; inspect `nulang_last_error`.
///
/// # Safety
/// `runtime` must be valid, `name` a null-terminated UTF-8 string, `ptr`
/// a valid function with exactly the provided C signature, and `params`
/// must contain `param_count` initialized valid CType entries if nonempty.
#[no_mangle]
pub unsafe extern "C" fn nulang_runtime_register_native_function(
    runtime: *mut NulangRuntime,
    name: *const c_char,
    ptr: *const c_void,
    params: *const super::marshal::CType,
    param_count: usize,
    ret: super::marshal::CType,
) -> i32 {
    if runtime.is_null() {
        return -1;
    }
    // SAFETY: caller supplies a live and exclusively accessed runtime.
    let rt = unsafe { &mut *runtime };
    rt.clear_error();
    if name.is_null() || ptr.is_null() || (param_count > 0 && params.is_null()) {
        rt.last_error = Some("invalid native callback arguments".to_string());
        return -1;
    }
    // SAFETY: caller guarantees a valid nul-terminated C string.
    let name_str = match unsafe { CStr::from_ptr(name).to_str() } {
        Ok(name) => name,
        Err(_) => {
            rt.last_error = Some("native callback name is not valid UTF-8".to_string());
            return -1;
        }
    };
    let params_slice = if param_count == 0 {
        &[]
    } else {
        // SAFETY: caller guarantees an initialized array of valid CType entries.
        unsafe { std::slice::from_raw_parts(params, param_count) }
    };
    let signature = super::marshal::Signature::new(params_slice.to_vec(), ret);
    // SAFETY: the embedding host guarantees that ptr matches signature and
    // that the callback remains live until the runtime is destroyed.
    let function =
        unsafe { super::native::NativeFunction::new(ptr, signature, None, name_str.to_string()) };
    let registration = rt
        .ffi_registry
        .lock()
        .map(|mut registry| registry.register(function));
    match registration {
        Ok(()) => 0,
        Err(_) => {
            rt.last_error = Some("embedded FFI registry lock failed".to_string());
            -1
        }
    }
}

/// Register a native C function in the legacy process-global registry.
///
/// This legacy entry point applies to standalone VMs using the global
/// registry; C embedding runtimes now use isolated per-runtime registries.
/// Use `nulang_runtime_register_native_function` for embedded callbacks.
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

    extern "C" fn scoped_value_one() -> i64 {
        101
    }

    extern "C" fn scoped_value_two() -> i64 {
        202
    }

    #[test]
    fn test_embedded_native_functions_are_isolated_per_runtime() {
        let first = nulang_runtime_new();
        let second = nulang_runtime_new();
        assert!(!first.is_null() && !second.is_null());

        let name = CString::new("nulang_scoped_test_value").unwrap();
        let rc_first = unsafe {
            nulang_runtime_register_native_function(
                first,
                name.as_ptr(),
                scoped_value_one as *const c_void,
                std::ptr::null(),
                0,
                crate::ffi::marshal::CType::I64,
            )
        };
        let rc_second = unsafe {
            nulang_runtime_register_native_function(
                second,
                name.as_ptr(),
                scoped_value_two as *const c_void,
                std::ptr::null(),
                0,
                crate::ffi::marshal::CType::I64,
            )
        };
        assert_eq!(rc_first, 0);
        assert_eq!(rc_second, 0);

        let source = CString::new(
            "extern \"__nulang_registered__\" { fn nulang_scoped_test_value() -> Int } nulang_scoped_test_value()",
        )
        .unwrap();
        let module_first = unsafe { nulang_compile(first, source.as_ptr()) };
        let module_second = unsafe { nulang_compile(second, source.as_ptr()) };
        assert!(module_first >= 0 && module_second >= 0);

        let first_result = unsafe { nulang_run(first, module_first) };
        let second_result = unsafe { nulang_run(second, module_second) };
        assert_eq!(nulang_value_int(first_result), 101);
        assert_eq!(nulang_value_int(second_result), 202);

        unsafe { nulang_runtime_free(first) };
        let second_again = unsafe { nulang_run(second, module_second) };
        assert_eq!(nulang_value_int(second_again), 202);
        unsafe { nulang_runtime_free(second) };
    }

    #[test]
    fn test_embedded_runtime_does_not_inherit_other_runtime_callbacks() {
        let owner = nulang_runtime_new();
        let unregistered = nulang_runtime_new();

        let name = CString::new("nulang_scoped_only_here").unwrap();
        let registered = unsafe {
            nulang_runtime_register_native_function(
                owner,
                name.as_ptr(),
                scoped_value_one as *const c_void,
                std::ptr::null(),
                0,
                crate::ffi::marshal::CType::I64,
            )
        };
        assert_eq!(registered, 0);

        let source = CString::new(
            "extern \"__nulang_registered__\" { fn nulang_scoped_only_here() -> Int } nulang_scoped_only_here()",
        )
        .unwrap();
        let handle = unsafe { nulang_compile(unregistered, source.as_ptr()) };
        assert!(handle >= 0);
        let value = unsafe { nulang_run(unregistered, handle) };
        assert!(nulang_value_is_nil(value));
        let error = unsafe { nulang_last_error(unregistered) };
        assert!(!error.is_null(), "unregistered callback must not execute");
        unsafe {
            nulang_runtime_free(owner);
            nulang_runtime_free(unregistered);
        }
    }

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
    fn test_cached_compile_handle_supports_module_operations() {
        let rt = nulang_runtime_new();
        assert!(!rt.is_null());

        let source =
            CString::new("fn len(s: String) -> Int { perform String.length(s) } len(\"\")")
                .unwrap();
        let first = unsafe { nulang_compile(rt, source.as_ptr()) };
        let cached = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(first >= 0 && cached >= 0);
        assert_ne!(first, cached);

        let text = CString::new("cached").unwrap();
        let arg = unsafe { nulang_module_string(rt, cached, text.as_ptr()) };
        assert!(!nulang_value_is_nil(arg));

        let args = [arg];
        let name = CString::new("len").unwrap();
        let result =
            unsafe { nulang_call_function(rt, cached, name.as_ptr(), args.as_ptr(), args.len()) };
        assert_eq!(nulang_value_int(result), 6);

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
    fn test_c_api_rejects_ambiguous_legacy_string_lookup_across_modules() {
        let rt = nulang_runtime_new();
        let a = CString::new("1 + 2").unwrap();
        let b = CString::new("3 + 4").unwrap();
        let module_a = unsafe { nulang_compile(rt, a.as_ptr()) };
        let module_b = unsafe { nulang_compile(rt, b.as_ptr()) };
        assert!(module_a >= 0 && module_b >= 0);

        // Both modules allocate their first string at the same string index.
        // Raw NulangValue bits alone cannot identify the owning module.
        let text_a = CString::new("from module a").unwrap();
        let text_b = CString::new("from module b").unwrap();
        let value_a = unsafe { nulang_module_string(rt, module_a, text_a.as_ptr()) };
        let value_b = unsafe { nulang_module_string(rt, module_b, text_b.as_ptr()) };
        assert_eq!(value_a.raw, value_b.raw);

        let legacy = unsafe { nulang_value_to_string(rt, value_a) };
        assert!(
            legacy.is_null(),
            "legacy lookup must not silently select the last module"
        );
        let error = unsafe { CStr::from_ptr(nulang_last_error(rt)) }
            .to_string_lossy()
            .into_owned();
        assert!(error.contains("ambiguous"), "{error}");

        let ptr_a = unsafe { nulang_value_to_string_in_module(rt, module_a, value_a) };
        assert!(!ptr_a.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(ptr_a) }.to_str().unwrap(),
            "from module a"
        );
        assert!(unsafe { nulang_free_string(rt, ptr_a) });

        let ptr_b = unsafe { nulang_value_to_string_in_module(rt, module_b, value_b) };
        assert!(!ptr_b.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(ptr_b) }.to_str().unwrap(),
            "from module b"
        );
        assert!(unsafe { nulang_free_string(rt, ptr_b) });

        let invalid = unsafe { nulang_value_to_string_in_module(rt, -1, value_a) };
        assert!(invalid.is_null());
        assert!(!unsafe { nulang_last_error(rt) }.is_null());
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_legacy_string_lookup_remains_valid_when_unambiguous() {
        let rt = nulang_runtime_new();
        let first = CString::new("7").unwrap();
        let second = CString::new("8").unwrap();
        let module_a = unsafe { nulang_compile(rt, first.as_ptr()) };
        let module_b = unsafe { nulang_compile(rt, second.as_ptr()) };
        assert!(module_a >= 0 && module_b >= 0);

        let shared = CString::new("identical").unwrap();
        let a = unsafe { nulang_module_string(rt, module_a, shared.as_ptr()) };
        let b = unsafe { nulang_module_string(rt, module_b, shared.as_ptr()) };
        assert_eq!(a.raw, b.raw);

        let ptr = unsafe { nulang_value_to_string(rt, a) };
        assert!(!ptr.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(ptr) }.to_str().unwrap(),
            "identical"
        );
        assert!(unsafe { nulang_free_string(rt, ptr) });
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
    fn test_c_api_rejects_forged_host_pointer_values() {
        let forged = NulangValue {
            raw: TAG_PTR | 0x1234,
        };
        assert_eq!(nulang_value_int(forged), 0);
        assert_eq!(nulang_value_float(forged), 0.0);
        assert!(!nulang_value_bool(forged));
        assert!(!nulang_value_is_nil(forged));
        assert!(!nulang_value_is_unit(forged));

        let rt = nulang_runtime_new();
        let ptr = unsafe { nulang_value_to_string(rt, forged) };
        assert!(ptr.is_null());
        let err = unsafe { nulang_last_error(rt) };
        assert!(!err.is_null());
        let message = unsafe { CStr::from_ptr(err) }.to_string_lossy();
        assert!(message.contains("pointer-tagged"));
        unsafe { nulang_runtime_free(rt) };
    }

    #[test]
    fn test_c_api_call_rejects_forged_host_pointer_argument() {
        let rt = nulang_runtime_new();
        let source = CString::new("fn id(x: Int) -> Int { x } id(0)").unwrap();
        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(handle >= 0);
        let name = CString::new("id").unwrap();
        let forged = [NulangValue {
            raw: TAG_PTR | 0x1234,
        }];
        let result = unsafe {
            nulang_call_function(rt, handle, name.as_ptr(), forged.as_ptr(), forged.len())
        };
        assert!(nulang_value_is_nil(result));
        let err = unsafe { nulang_last_error(rt) };
        assert!(!err.is_null());
        let message = unsafe { CStr::from_ptr(err) }.to_string_lossy();
        assert!(message.contains("pointer-tagged"));
        unsafe { nulang_runtime_free(rt) };
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
    fn test_c_api_call_function_uses_bytecode_offset_for_non_first_function() {
        let rt = nulang_runtime_new();
        let source =
            CString::new("fn first() -> Int { 7 } fn second() -> Int { 42 } first()").unwrap();

        let handle = unsafe { nulang_compile(rt, source.as_ptr()) };
        assert!(handle >= 0, "compile failed");

        // Prove this fixture would fail if the function-table index were
        // accidentally passed to VM::call_function as a bytecode PC.
        let runtime = unsafe { &*rt };
        let module_index = runtime.module_handles[handle as usize];
        let module = &runtime.modules[module_index];
        let offset = module
            .function_offset_by_name("second")
            .expect("second function offset");
        let table_index = module
            .function_table
            .iter()
            .position(|&candidate| candidate == offset)
            .expect("second function table entry");
        assert_ne!(
            offset, table_index,
            "regression fixture requires bytecode offset != function-table index"
        );

        let name = CString::new("second").unwrap();
        let result =
            unsafe { nulang_call_function(rt, handle, name.as_ptr(), std::ptr::null(), 0) };
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
