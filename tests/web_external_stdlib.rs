//! A source-built CLI must build a web app from outside the Nulang checkout.
use std::process::Command;

#[test]
fn web_build_resolves_imported_html_type_from_an_external_project() {
    let project = std::env::temp_dir().join(format!(
        "nulang_web_stdlib_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("valid clock")
            .as_nanos()
    ));
    let src = project.join("src");
    let static_dir = project.join("static");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&static_dir).unwrap();

    std::fs::write(
        project.join("Nulang.toml"),
        "[package]\nname = \"external-web\"\nversion = \"0.1.0\"\n\n[web]\nstatic_dir = \"static\"\noutput_dir = \"dist\"\n",
    ).unwrap();
    std::fs::write(
        src.join("main.nula"),
        r#"import stdlib::web::html
import stdlib::web::types

@placement(static)
fn home() -> Html {
    document(
        head([title("External")]),
        el("body", [("class", text("syn-root")), ("data-syn-theme", text("studio"))], [
            el("h1", [], [text("Works")])
        ])
    )
}

app "external" {
    route "GET" "/" -> home
}
"#,
    )
    .unwrap();
    std::fs::write(static_dir.join("styles.css"), "body { color: black; }").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["nula", "build", "--web"])
        .current_dir(&project)
        .env_remove("NULANG_STDLIB")
        .output()
        .expect("start Nulang package builder");

    let html = project.join("dist/index.html");
    let stylesheet = project.join("dist/styles.css");
    let result = std::panic::catch_unwind(|| {
        assert!(
            output.status.success(),
            "web build failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let contents = std::fs::read_to_string(&html).expect("rendered index.html");
        assert!(contents.contains("class=\"syn-root\""));
        assert!(contents.contains("data-syn-theme=\"studio\""));
        assert!(contents.contains("Works"));
        assert_eq!(
            std::fs::read_to_string(&stylesheet).expect("copied static CSS"),
            "body { color: black; }"
        );
    });

    let _ = std::fs::remove_dir_all(&project);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
