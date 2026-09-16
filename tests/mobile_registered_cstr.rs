use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::{Mutex, OnceLock};

use nulang::ffi::c_api::{
    nulang_compile, nulang_last_error, nulang_register_native_function, nulang_run,
    nulang_runtime_free, nulang_runtime_new_interpreter,
};
use nulang::ffi::CType;

static CAPTURED: OnceLock<Mutex<Option<String>>> = OnceLock::new();

extern "C" fn capture_mobile_json(value: *const c_char) {
    let text = if value.is_null() {
        None
    } else {
        // SAFETY: the VM's CStr marshalling path provides a valid temporary
        // null-terminated string for the duration of this callback.
        Some(
            unsafe { CStr::from_ptr(value) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    *CAPTURED.get_or_init(|| Mutex::new(None)).lock().unwrap() = text;
}

#[test]
fn interpreter_mobile_profile_delivers_nulang_strings_to_registered_cstr_callbacks() {
    *CAPTURED.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;

    let symbol = CString::new("nulang_mobile_test_capture_json").unwrap();
    let params = [CType::CStr];
    // SAFETY: capture_mobile_json has the exact C ABI described by params/ret
    // and remains valid for the process lifetime.
    let registered = unsafe {
        nulang_register_native_function(
            symbol.as_ptr(),
            capture_mobile_json as *const () as *const c_void,
            params.as_ptr(),
            params.len(),
            CType::Unit,
        )
    };
    assert_eq!(registered, 0);

    let runtime = nulang_runtime_new_interpreter();
    assert!(!runtime.is_null());

    let source = CString::new(
        r#"
        extern "__nulang_registered__" {
          fn nulang_mobile_test_capture_json(value: String) -> Unit
        }
        nulang_mobile_test_capture_json("{\"protocol\":\"nulang-ui/1\"}")
        "#,
    )
    .unwrap();

    // SAFETY: runtime and source remain valid for this call.
    let handle = unsafe { nulang_compile(runtime, source.as_ptr()) };
    if handle < 0 {
        // SAFETY: runtime remains alive and owns the returned error pointer.
        let error = unsafe { nulang_last_error(runtime) };
        let message = if error.is_null() {
            "unknown compile error".to_string()
        } else {
            // SAFETY: non-null error is a runtime-owned C string.
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        // SAFETY: runtime was created above and is freed exactly once.
        unsafe { nulang_runtime_free(runtime) };
        panic!("mobile callback fixture failed to compile: {message}");
    }

    // SAFETY: runtime and returned module handle are valid.
    let _ = unsafe { nulang_run(runtime, handle) };
    // SAFETY: runtime remains alive and owns any error pointer.
    let run_error = unsafe { nulang_last_error(runtime) };
    if !run_error.is_null() {
        // SAFETY: non-null error is a runtime-owned C string.
        let message = unsafe { CStr::from_ptr(run_error) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: runtime was created above and is freed exactly once.
        unsafe { nulang_runtime_free(runtime) };
        panic!("mobile callback fixture failed at runtime: {message}");
    }

    assert_eq!(
        CAPTURED.get().unwrap().lock().unwrap().as_deref(),
        Some("{\"protocol\":\"nulang-ui/1\"}")
    );

    // SAFETY: runtime was created above and is freed exactly once.
    unsafe { nulang_runtime_free(runtime) };
}
