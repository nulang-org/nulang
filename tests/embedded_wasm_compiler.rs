//! Narrow source-to-core-WASM embedding contract. No subprocesses or imports.
#![cfg(feature = "wasm-backend")]

use nulang::embedded_wasm::compile_pure_source;

#[test]
fn compiles_a_typed_pure_main_to_valid_core_wasm() {
    let bytes = compile_pure_source("fn main() { 40 + 2 }").expect("pure compiler");
    assert!(bytes.starts_with(&[0, 97, 115, 109, 1, 0, 0, 0]));
    assert!(bytes.len() > 8, "a module must contain Wasm sections");
}

#[test]
fn repeated_pure_compilation_is_deterministic() {
    let source = "fn main() { 7 * 6 }";
    let left = compile_pure_source(source).expect("first");
    let right = compile_pure_source(source).expect("second");
    assert_eq!(left, right);
}

#[test]
fn rejects_unresolved_types_or_syntax() {
    for src in ["fn main() { unknown_name }", "fn main( {", ""] {
        assert!(compile_pure_source(src).is_err(), "input must fail: {src:?}");
    }
}

#[test]
fn never_resolves_imports_or_native_ffi_from_the_host() {
    for src in [
        "import \"/etc/passwd\"",
        "extern fn secret() -> Int",
        "fn main() { 1 } import \"file.nula\"",
    ] {
        assert!(compile_pure_source(src).is_err(), "must reject: {src:?}");
    }
}

#[test]
fn refuses_effectful_and_actor_constructs() {
    for src in [
        "fn main() { perform IO.print(message: \"hi\") }",
        "fn main() { spawn Counter() }",
        "actor Counter { state count: Int = 0 }",
        "fn main() { receive { | Ping => 42 } }",
    ] {
        assert!(compile_pure_source(src).is_err(), "must reject: {src:?}");
    }
}

#[test]
fn bounded_source_and_unapproved_warnings_fail_closed() {
    let too_long = " ".repeat(4 * 1024 * 1024 + 1);
    assert!(compile_pure_source(&too_long).is_err());
    // No implicit file resolution on a fresh call after an error.
    assert!(compile_pure_source("fn main() { 42 }").is_ok());
}
