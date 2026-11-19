//! The generated C surface is reproducible and compiles with a strict C11 compiler.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn checked_in_header_matches_ffi_rust_exports() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("python3")
        .arg(root.join("scripts/generate_c_header.py"))
        .arg("--check")
        .current_dir(root)
        .output()
        .expect("invoke C header generator");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
#[test]
fn header_supports_c11_with_expected_layout_and_function_signatures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut cc = Command::new("cc")
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pedantic",
            "-fsyntax-only",
            "-x",
            "c",
            "-",
        ])
        .arg("-I")
        .arg(root.join("include"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("C compiler is required to check the ABI");
    cc.stdin.take().unwrap().write_all(br#"
#include "nulang.h"
_Static_assert(sizeof(NulangValue) == sizeof(uint64_t), "NulangValue must be one word");
_Static_assert(_Alignof(NulangValue) == _Alignof(uint64_t), "C value alignment");
_Static_assert(NULANG_CTYPE_I64 == 0, "C enum ABI");
_Static_assert(NULANG_CTYPE_VALUE == 6, "C enum ABI");
static void signatures(void) {
    int64_t (*compile_fn)(NulangRuntime *, const char *) = nulang_compile;
    int (*register_fn)(const char *, const void *, const NulangCType *, size_t, NulangCType) = nulang_register_native_function;
    (void)compile_fn;
    (void)register_fn;
}
"#).unwrap();
    let out = cc.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
#[test]
fn public_header_can_be_consumed_by_cpp11() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut cpp = Command::new("c++")
        .args([
            "-std=c++11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-x",
            "c++",
            "-",
        ])
        .arg("-I")
        .arg(root.join("include"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("C++ compiler is required to validate the public header");
    cpp.stdin
        .take()
        .unwrap()
        .write_all(
            br#"
#include "nulang.h"
#include <type_traits>
static_assert(sizeof(NulangValue) == sizeof(uint64_t), "C++ layout");
static_assert(std::is_same<decltype(&nulang_compile),
                           int64_t (*)(NulangRuntime *, const char *)>::value,
                           "C++ function signature");
"#,
        )
        .unwrap();
    let out = cpp.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
