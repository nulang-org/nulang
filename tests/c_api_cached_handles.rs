use std::ffi::CString;

use nulang::ffi::c_api::{
    nulang_call_function, nulang_compile, nulang_module_string, nulang_runtime_free,
    nulang_runtime_new, nulang_value_int,
};

#[test]
fn cached_public_handle_works_across_c_api_module_operations() {
    let runtime = nulang_runtime_new();
    assert!(!runtime.is_null());

    let source = CString::new(
        "fn len(s: String) -> Int { perform String.length(s) } len(\"\")",
    )
    .unwrap();
    // SAFETY: runtime and source remain valid for the calls below.
    let first = unsafe { nulang_compile(runtime, source.as_ptr()) };
    let cached = unsafe { nulang_compile(runtime, source.as_ptr()) };
    assert!(first >= 0 && cached >= 0);
    assert_ne!(first, cached, "public compile handles retain fresh identity");

    let text = CString::new("public cached handle").unwrap();
    // SAFETY: runtime, cached handle, and text are valid.
    let argument = unsafe { nulang_module_string(runtime, cached, text.as_ptr()) };

    let function = CString::new("len").unwrap();
    let arguments = [argument];
    // SAFETY: the argument array and function name outlive this call.
    let result = unsafe {
        nulang_call_function(
            runtime,
            cached,
            function.as_ptr(),
            arguments.as_ptr(),
            arguments.len(),
        )
    };
    assert_eq!(nulang_value_int(result), 20);

    // SAFETY: runtime was allocated by nulang_runtime_new and is freed once.
    unsafe { nulang_runtime_free(runtime) };
}
