//! CLI acceptance coverage for the static/dynamic deployment boundary.

use nulang::web::ir::DeploymentIr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Site(PathBuf);

impl Site {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "nulang-static-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(
            root.join("Nulang.toml"),
            r#"[package]
name = "static-fixture"
version = "0.1.0"
entry = "src/main.nula"
[web]
static_dir = "public"
output_dir = "dist"
"#,
        )
        .unwrap();
        Self(root)
    }

    fn source(&self, contents: &str) {
        std::fs::write(self.0.join("src/main.nula"), contents).unwrap();
    }

    fn build(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_nulang"))
            .current_dir(&self.0)
            .env(
                "NULANG_STDLIB",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
            )
            .env_remove("CARGO_BIN_EXE_nulang")
            .env("PATH", "")
            .args(["nula", "build", "--web"])
            .output()
            .unwrap()
    }

    fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.0.join(path)).unwrap()
    }

    fn ir(&self) -> DeploymentIr {
        serde_json::from_slice(&self.read("dist/nulang-app.ir.json")).unwrap()
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const STATIC: &str = r#"
import stdlib::web::html
import stdlib::web::types
fn home() -> Html { text("hello static") }
app "site" {
    route "GET" "/" -> home
}
"#;

#[test]
fn mixed_build_emits_only_static_get_routes_and_consistent_metadata() {
    let site = Site::new();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types

fn home() -> Html { text("hello static") }
fn helper() -> String { perform Web.cookie("session") }
fn account() -> Html { text(helper()) }
fn mutate() -> Html {
    perform IO.print("MUTATION_HANDLER_RAN")
    text("incorrect")
}
fn user() -> Html { text(perform Web.param("name")) }
fn query(limit: Int from query) -> Html { text(perform Int.to_string(limit)) }
@placement(server)
fn on_demand() -> Html { text("server only") }
app "mixed" {
    route "GET" "/" -> home
    route "POST" "/" -> mutate
    route "GET" "/account" -> account
    route "GET" "/users/:name" -> user
    route "GET" "/query" -> query
    route "GET" "/server" -> on_demand
}
"#,
    );
    let output = site.build();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("MUTATION_HANDLER_RAN"));
    success(output);
    let home = site.read("dist/index.html");
    assert_eq!(home, b"hello static");
    let ir = site.ir();
    assert_eq!(ir.routes.len(), 6);
    for route in ir.routes {
        if route.method == "GET" && route.path == "/" {
            assert_eq!(route.placement, "static");
            assert_eq!(route.artifact.as_deref(), Some("index.html"));
        } else {
            assert_eq!(route.placement, "server", "{}", route.path);
            assert_eq!(route.artifact, None);
        }
    }
    for path in ["account", "users", "query", "server"] {
        assert!(!site.0.join("dist").join(path).exists());
    }
}

#[test]
fn repeated_builds_are_identical_and_removed_routes_do_not_survive() {
    let site = Site::new();
    site.source(STATIC);
    success(site.build());
    let old_html = site.read("dist/index.html");
    let old_ir = site.read("dist/nulang-app.ir.json");
    success(site.build());
    assert_eq!(site.read("dist/index.html"), old_html);
    assert_eq!(site.read("dist/nulang-app.ir.json"), old_ir);

    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
fn home() -> Html { text(perform Web.cookie("session")) }
app "site" {
    route "GET" "/" -> home
}
"#,
    );
    success(site.build());
    assert!(!site.0.join("dist/index.html").exists());
    assert_eq!(site.ir().routes[0].placement, "server");
}

#[test]
fn failed_build_preserves_previous_output_and_rejects_asset_collisions() {
    let site = Site::new();
    site.source(STATIC);
    success(site.build());
    let old = site.read("dist/index.html");
    std::fs::write(site.0.join("public/index.html"), "unintended override").unwrap();
    let failed = site.build();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("static asset conflicts"));
    assert_eq!(site.read("dist/index.html"), old);
}

#[test]
fn explicit_static_routes_cannot_read_request_state() {
    let site = Site::new();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
fn helper() -> String { perform Web.cookie("session") }
@placement(static)
fn home() -> Html { text(helper()) }
app "site" {
    route "GET" "/" -> home
}
"#,
    );
    let output = site.build();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("static"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!site.0.join("dist").exists());
}

#[test]
fn aliases_cannot_overwrite_the_same_generated_artifact() {
    let site = Site::new();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
fn home() -> Html { text("a") }
fn other() -> Html { text("b") }
app "site" {
    route "GET" "/docs" -> home
    route "GET" "/docs/" -> other
}
"#,
    );
    let output = site.build();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("conflicts"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!site.0.join("dist").exists());
}

#[test]
fn non_generated_output_is_not_replaced() {
    let site = Site::new();
    site.source(STATIC);
    std::fs::create_dir(site.0.join("dist")).unwrap();
    std::fs::write(site.0.join("dist/user.txt"), "keep").unwrap();
    let output = site.build();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("non-generated output directory"));
    assert_eq!(site.read("dist/user.txt"), b"keep");
}

#[test]
fn route_registration_cannot_execute_ambient_effects() {
    let site = Site::new();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
fn home() -> Html { text("hello") }
fn main() {
    perform IO.print("BUILD_STARTUP_EFFECT_RAN")
    perform Web.route("GET", "/", home)
}
"#,
    );
    let output = site.build();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("web route registration failed") && stderr.contains("disallows IO.print"),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("BUILD_STARTUP_EFFECT_RAN"));
    assert!(!site.0.join("dist").exists());
}
