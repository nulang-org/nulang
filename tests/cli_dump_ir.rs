//! Inspection runs the checked compiler pipeline, never the program.
use std::path::Path;
use std::process::Command;

fn fixture() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/examples/01_hello.nula")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(args)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .output()
        .expect("run nulang")
}

#[test]
fn inspection_emits_checked_ast_hir_and_mir_without_running() {
    for stage in ["ast", "hir", "mir"] {
        let out = run(&["--dump-ir", stage, fixture()]);
        assert!(
            out.status.success(),
            "{stage}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.starts_with(&format!("=== {} ===", stage.to_uppercase())),
            "{stdout}"
        );
        assert!(stdout.contains("Module"), "{stage}: {stdout}");
        assert!(
            !stdout.contains("Hello, Nulang!\nHello, World!"),
            "the program must not run"
        );
    }
}

#[test]
fn inspection_rejects_unsupported_stage_before_execution() {
    let out = run(&["--dump-ir", "native", fixture()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid IR stage"));
}

#[test]
fn inspection_rejects_type_errors_without_dumping_ir() {
    let out = run(&[
        "--dump-ir",
        "hir",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/conformance/behavior/typeclass_03_missing_impl_reject.nula"
        ),
    ]);
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("=== HIR ==="));
}

#[test]
fn mir_inspection_does_not_execute_filesystem_effects() {
    let dir =
        std::env::temp_dir().join(format!("nulang_dump_ir_side_effect_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("side_effect.nula");
    let output_path = dir.join("must_not_exist.txt");
    // Use a clean path so a previous invocation cannot produce a false positive.
    let _ = std::fs::remove_file(&output_path);
    std::fs::write(
        &source,
        format!(
            "perform FS.write({:?}, \"unsafe runtime side effect\")\n",
            output_path.to_str().unwrap()
        ),
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--dump-ir")
        .arg("mir")
        .arg("--with")
        .arg("fs")
        .arg(&source)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "MIR inspection failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !output_path.exists(),
        "IR inspection executed a filesystem effect"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn inspection_rejects_json_diagnostics_mode() {
    let out = run(&["--dump-ir", "ast", "--json", fixture()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("without other execution modes"));
}

#[test]
fn inspection_rejects_init_without_creating_a_directory() {
    let dir = std::env::temp_dir().join(format!(
        "nulang_dump_ir_init_must_not_run_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["--dump-ir", "ast", "init"])
        .arg(&dir)
        .arg(fixture())
        .output()
        .expect("run nulang with incompatible init mode");
    // Clean up even if the current implementation wrongly creates the dir.
    let created = dir.exists();
    if created {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    assert!(!out.status.success(), "init must be rejected");
    assert!(!created, "--dump-ir must never create project directories");
}

#[test]
fn inspection_rejects_watch_mode_before_running() {
    let out = run(&["--dump-ir", "mir", "--watch", fixture(), fixture()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("without other execution modes"));
}
