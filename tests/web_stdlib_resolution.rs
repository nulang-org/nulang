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

#[test]
fn generated_resource_and_model_build_outside_checkout() {
    let dir = std::env::temp_dir().join(format!(
        "nulang_web_resource_model_external_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create external test root");
    let cli = env!("CARGO_BIN_EXE_nulang");

    let scaffold = Command::new(cli)
        .args(["nula", "new", "catalog", "--template", "web"])
        .current_dir(&dir)
        .env_remove("NULANG_STDLIB")
        .output()
        .expect("scaffold web starter");
    assert!(
        scaffold.status.success(),
        "scaffold failed: {}",
        String::from_utf8_lossy(&scaffold.stderr)
    );

    let root = dir.join("catalog");
    for args in [
        vec!["nula", "generate", "resource", "BlogPost"],
        vec![
            "nula",
            "generate",
            "model",
            "BlogPost",
            "--field",
            "title:String",
            "--field",
            "published:Bool",
        ],
    ] {
        let result = Command::new(cli)
            .args(args)
            .current_dir(&root)
            .env_remove("NULANG_STDLIB")
            .output()
            .expect("generate resource or model");
        assert!(
            result.status.success(),
            "generation failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let model = fs::read_to_string(root.join("src/resources/blog_post.nula"))
        .expect("typed resource model");
    assert!(model.contains("pub type BlogPostInput"));
    assert!(model.contains("can_read_blog_post"));
    assert!(model.contains("-> Bool { false }"));

    let build = Command::new(cli)
        .args(["nula", "build", "--web"])
        .current_dir(&root)
        .env_remove("NULANG_STDLIB")
        .output()
        .expect("compile generated app outside checkout");
    assert!(
        build.status.success(),
        "generated web app failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(root.join("dist/blog_posts/index.html").is_file());
    assert!(root.join("dist/blog_posts/new/index.html").is_file());
    let deployment: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("dist/nulang-app.ir.json")).expect("web deployment IR"),
    )
    .expect("parse IR");
    let routes = deployment["routes"].as_array().expect("routes list");
    for expected in ["/blog_posts", "/blog_posts/new"] {
        assert!(
            routes
                .iter()
                .any(|route| route["method"] == "GET" && route["path"] == expected),
            "missing compiler-owned route {expected}: {routes:?}"
        );
    }
    fs::remove_dir_all(dir).expect("remove generated fixture");
}
