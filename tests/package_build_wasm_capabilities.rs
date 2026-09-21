#![cfg(feature = "wasm-backend")]

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_package_dir() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "nulang_build_wasm_capability_{}_{}",
        std::process::id(),
        nonce
    ))
}

#[test]
fn build_wasm_forwards_manifest_capabilities() {
    let dir = temp_package_dir();
    fs::create_dir_all(dir.join("src")).expect("create package source directory");

    fs::write(
        dir.join("Nulang.toml"),
        r#"[package]
name = "wasm-capability-test"
version = "0.1.0"
entry = "src/main.nula"
capabilities = ["net"]

[dependencies]
"#,
    )
    .expect("write manifest");

    fs::write(
        dir.join("src/main.nula"),
        r#"fn main() {
    perform Http.get("https://example.com")
}
"#,
    )
    .expect("write source");

    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["nula", "build-wasm"])
        .current_dir(&dir)
        .output()
        .expect("run nula build-wasm");

    assert!(
        output.status.success(),
        "build-wasm should honor manifest capabilities\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        dir.join(".nula/dist/wasm-capability-test.wasm").is_file(),
        "build-wasm should emit the package wasm artifact"
    );

    let _ = fs::remove_dir_all(&dir);
}
