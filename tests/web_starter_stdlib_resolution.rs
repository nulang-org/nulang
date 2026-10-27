//! A source-built CLI must build the untouched web starter from outside the
//! Nulang checkout, without a process-global NULANG_STDLIB override.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn web_starter_build_resolves_stdlib_without_env_from_external_cwd() {
    let scratch = std::env::temp_dir().join(format!(
        "nulang_web_stdlib_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&scratch).expect("create scratch");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_nulang"));

    let create = Command::new(&executable)
        .args(["nula", "new", "starter", "--template", "web"])
        .current_dir(&scratch)
        .env_remove("NULANG_STDLIB")
        .output()
        .expect("run nula new");
    assert!(
        create.status.success(),
        "starter generation failed: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    let project = scratch.join("starter");
    let build = Command::new(&executable)
        .args(["nula", "build", "--web"])
        .current_dir(&project)
        .env_remove("NULANG_STDLIB")
        .output()
        .expect("run nula build --web");
    assert!(
        build.status.success(),
        "web starter failed without NULANG_STDLIB: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(project.join("dist/nulang-app.ir.json").exists());
    assert!(project.join("dist/index.html").exists());
    std::fs::remove_dir_all(&scratch).expect("remove scratch");
}
