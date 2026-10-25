//! Regression: a CLI built from source resolves stdlib while serving a web project
//! outside the compiler checkout without requiring a global NULANG_STDLIB.
use std::fs;
use std::process::Command;

#[test]
fn web_starter_builds_without_explicit_stdlib_environment() {
    let dir = std::env::temp_dir().join(format!(
        "nulang_web_default_stdlib_{}_{}",
        std::process::id(),
        std::thread::current()
            .name()
            .unwrap_or("thread")
            .replace(':', "_")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let cli = env!("CARGO_BIN_EXE_nulang");

    let scaffold = Command::new(cli)
        .args(["nula", "new", "sample", "--template", "web"])
        .current_dir(&dir)
        .env_remove("NULANG_STDLIB")
        .output()
        .expect("execute nula new");
    assert!(
        scaffold.status.success(),
        "failed to scaffold:\n{}",
        String::from_utf8_lossy(&scaffold.stderr)
    );

    let build = Command::new(cli)
        .args(["nula", "build", "--web"])
        .current_dir(dir.join("sample"))
        .env_remove("NULANG_STDLIB")
        .output()
        .expect("execute nula build --web");

    assert!(
        build.status.success(),
        "web starter must resolve stdlib without explicit environment; stderr:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(dir.join("sample/dist/index.html").exists());
    assert!(dir.join("sample/dist/nulang-app.ir.json").exists());

    // An explicitly set stdlib path must not silently fall back to a
    // different copy: a typo in deployment configuration is an error.
    let invalid_override = Command::new(cli)
        .args(["nula", "build", "--web"])
        .current_dir(dir.join("sample"))
        .env("NULANG_STDLIB", dir.join("missing-stdlib"))
        .output()
        .expect("execute invalid stdlib override");
    assert!(
        !invalid_override.status.success(),
        "an explicit invalid standard library root must not be ignored"
    );

    fs::remove_dir_all(dir).unwrap();
}
