//! Check the public Unpoly compatibility module through the same CLI and
//! standard-library resolution path an application uses.
use std::path::Path;
use std::process::Command;

#[test]
fn unpoly_example_typechecks_without_a_custom_browser_runtime() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--check")
        .arg(root.join("examples/unpoly-compat/src/main.nula"))
        .env("NULANG_STDLIB", root.join("src/stdlib"))
        .output()
        .expect("run Nulang compiler");
    assert!(
        output.status.success(),
        "Unpoly example must typecheck\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
