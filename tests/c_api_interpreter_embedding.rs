use std::ffi::{CStr, CString};

use nulang::ffi::c_api::{
    nulang_call_function, nulang_compile, nulang_last_error, nulang_load_nbc, nulang_runtime_free,
    nulang_runtime_new_interpreter, nulang_value_int, nulang_value_int_new,
};

#[test]
fn interpreter_runtime_compiles_and_calls_exported_function() {
    let runtime = nulang_runtime_new_interpreter();
    assert!(!runtime.is_null());

    let source = CString::new("fn add(a: Int, b: Int) -> Int { a + b } add(0, 0)").unwrap();
    // SAFETY: runtime and source remain valid for the call.
    let handle = unsafe { nulang_compile(runtime, source.as_ptr()) };
    assert!(handle >= 0);

    let name = CString::new("add").unwrap();
    let arguments = [nulang_value_int_new(19), nulang_value_int_new(23)];
    // SAFETY: function name and argument storage outlive this call.
    let result = unsafe {
        nulang_call_function(
            runtime,
            handle,
            name.as_ptr(),
            arguments.as_ptr(),
            arguments.len(),
        )
    };
    assert_eq!(nulang_value_int(result), 42);

    // SAFETY: runtime was allocated by the matching constructor and is freed once.
    unsafe { nulang_runtime_free(runtime) };
}

#[test]
fn invalid_nbc_is_rejected_through_public_embedding_api() {
    let runtime = nulang_runtime_new_interpreter();
    assert!(!runtime.is_null());

    let invalid = b"not-an-nbc";
    // SAFETY: runtime is valid and invalid points to invalid.len() readable bytes.
    let handle = unsafe { nulang_load_nbc(runtime, invalid.as_ptr(), invalid.len()) };
    assert_eq!(handle, -1);

    // SAFETY: runtime remains valid and owns the returned error string.
    let error = unsafe { nulang_last_error(runtime) };
    assert!(!error.is_null());
    // SAFETY: nulang_last_error returns a null-terminated runtime-owned string.
    let message = unsafe { CStr::from_ptr(error) }.to_string_lossy();
    assert!(!message.is_empty());

    // SAFETY: runtime was allocated by the matching constructor and is freed once.
    unsafe { nulang_runtime_free(runtime) };
}
