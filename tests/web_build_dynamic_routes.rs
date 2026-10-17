//! The Web IR build must not execute server-only handlers as static pages.
//! This integration test exercises the actual CLI and compiler against a
//! compiler-bound route example rather than constructing fake IR in isolation.

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "nulang_web_ir_build_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ));
    std::fs::create_dir_all(path.join("src")).expect("create fixture directory");
    path
}

#[test]
fn build_web_emits_contract_for_server_routes_without_static_rendering() {
    let root = temp_dir();
    std::fs::write(
        root.join("Nulang.toml"),
        include_str!("../examples/typed-api/Nulang.toml"),
    )
    .expect("write manifest");
    std::fs::write(
        root.join("src/main.nula"),
        include_str!("../examples/typed-api/src/main.nula"),
    )
    .expect("write source");

    let result = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["nula", "build", "--web"])
        .current_dir(&root)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .output()
        .expect("run nula web build");

    let output = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.status.success(), "web build must succeed: {output}");

    let ir: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("dist/nulang-app.ir.json"))
            .expect("IR is emitted"))
            .expect("IR is valid JSON");
    assert_eq!(ir["version"], 1);
    let routes = ir["routes"].as_array().expect("routes");
    assert_eq!(routes.len(), 3, "{ir}");
    assert!(routes.iter().all(|route| route["placement"] == "server"));
    let show = routes
        .iter()
        .find(|route| route["handler"] == "show_item")
        .expect("typed show_item route");
    assert_eq!(show["bindings"][0]["source"], "path");
    assert_eq!(show["bindings"][0]["source_name"], "item_id");
    assert_eq!(show["bindings"][1]["source"], "query");
    assert_eq!(show["bindings"][1]["source_name"], "details");

    // The dynamic route must remain in the IR but must not be executed
    // without request parameters or written as a public static file.
    assert!(!root.join("dist/items/{item_id: Int}/index.html").exists());
    assert!(!root.join("dist/items/index.html").exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn build_web_still_renders_pure_static_routes() {
    let root = temp_dir();
    std::fs::write(
        root.join("Nulang.toml"),
        include_str!("../examples/hello-web/Nulang.toml"),
    )
    .expect("write manifest");
    std::fs::write(
        root.join("src/main.nula"),
        include_str!("../examples/hello-web/src/main.nula"),
    )
    .expect("write source");

    let result = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["nula", "build", "--web"])
        .current_dir(&root)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .output()
        .expect("run nula web build");

    assert!(
        result.status.success(),
        "static web build must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );

    let page = std::fs::read_to_string(root.join("dist/index.html"))
        .expect("static home is rendered");
    assert!(page.contains("Hello, Nulang Web!"), "{page}");

    let ir: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("dist/nulang-app.ir.json"))
            .expect("IR is emitted"))
            .expect("IR is valid JSON");
    assert_eq!(ir["routes"][0]["placement"], "static");
    let _ = std::fs::remove_dir_all(&root);
}
