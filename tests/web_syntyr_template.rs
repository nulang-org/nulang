//! Offline scaffold/build regression for the opt-in Syntyr Web template.

use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nulang_syntyr_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_nula(dir: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .current_dir(dir)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        // Exercise the installed-CLI path rather than Cargo's test-only
        // binary override. The stylesheet preset must not require Cargo.
        .env_remove("CARGO_BIN_EXE_nulang")
        .env("PATH", "")
        .arg("nula")
        .args(args)
        .output()
        .expect("run built Nulang CLI");
    assert!(
        output.status.success(),
        "CLI {:?} failed:\n{}\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_web_syntyr_scaffold_and_build_are_self_contained() {
    let temp = TempDir::new();
    run_nula(
        &temp.0,
        &["new", "styled", "--template", "web", "--style", "syntyr"],
    );

    let app = temp.0.join("styled");
    let manifest = std::fs::read_to_string(app.join("Nulang.toml")).unwrap();
    assert!(manifest.contains("name = \"styled\""));
    assert!(manifest.contains("[web]"));
    assert!(manifest.contains("static_dir = \"public\""));

    let assets: &[(&str, &[u8])] = &[
        (
            "syntyr.core.css",
            include_bytes!("../src/package/templates/syntyr/syntyr.core.css"),
        ),
        (
            "syntyr.semantic.css",
            include_bytes!("../src/package/templates/syntyr/syntyr.semantic.css"),
        ),
        (
            "SYNTYR-LICENSE.txt",
            include_bytes!("../src/package/templates/syntyr/SYNTYR-LICENSE"),
        ),
    ];
    for (file, bytes) in assets {
        assert_eq!(
            std::fs::read(app.join("public").join(file)).unwrap(),
            *bytes
        );
    }

    run_nula(&app, &["build", "--web"]);

    let html = std::fs::read_to_string(app.join("dist/index.html")).unwrap();
    for required in [
        "lang=\"en\"",
        "data-syn-theme=\"studio\"",
        "href=\"/syntyr.core.css\"",
        "href=\"/syntyr.semantic.css\"",
        "class=\"syn-root syn-semantic app-shell\"",
        "name=\"__nulang_action\"",
        "value=\"add_todo\"",
        "method=\"POST\"",
        "for=\"todo-title\"",
    ] {
        assert!(html.contains(required), "missing {required:?}: {html}");
    }
    assert!(!html.contains("https://"));
    assert!(!html.contains("http://"));
    for (file, bytes) in assets {
        assert_eq!(std::fs::read(app.join("dist").join(file)).unwrap(), *bytes);
    }

    // The regular web template must remain independent of Syntyr.
    run_nula(&temp.0, &["new", "plain", "--template", "web"]);
    let plain = temp.0.join("plain");
    assert!(plain.join("public/style.css").is_file());
    assert!(!plain.join("public/syntyr.core.css").exists());
    run_nula(&plain, &["build", "--web"]);
    assert!(plain.join("dist/index.html").is_file());
    assert!(!plain.join("dist/syntyr.core.css").exists());
}
