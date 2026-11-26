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

const PARAMETERIZED: &str = r#"
import stdlib::web::html
import stdlib::web::types
fn article(page: Int, slug: String) -> Html {
    text(perform String.concat(perform String.concat(slug, ":"), perform Int.to_string(page)))
}
app "articles" {
    route "GET" "/articles/{slug}/{page: Int}" -> article
}
"#;

fn prerender(site: &Site, entries: &str) {
    let path = site.0.join("Nulang.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    let prefix = original.split("[web.prerender]").next().unwrap();
    std::fs::write(path, format!("{prefix}\n[web.prerender]\n{entries}\n")).unwrap();
}

#[test]
fn configured_parameterized_pages_are_finite_and_reproducible() {
    let site = Site::new();
    site.source(PARAMETERIZED);
    prerender(
        &site,
        r#""/articles/{slug}/{page: Int}" = ["/articles/z/2", "/articles/a/1"]"#,
    );
    success(site.build());
    assert_eq!(site.read("dist/articles/a/1/index.html"), b"a:1");
    assert_eq!(site.read("dist/articles/z/2/index.html"), b"z:2");
    let ir = site.ir();
    assert_eq!(ir.routes.len(), 2);
    assert_eq!(ir.routes[0].path, "/articles/a/1");
    assert_eq!(ir.routes[1].path, "/articles/z/2");
    assert!(ir
        .routes
        .iter()
        .all(|r| r.placement == "static" && r.artifact.is_some()));
    let old = site.read("dist/nulang-app.ir.json");
    prerender(
        &site,
        r#""/articles/{slug}/{page: Int}" = ["/articles/a/1", "/articles/z/2"]"#,
    );
    success(site.build());
    assert_eq!(site.read("dist/nulang-app.ir.json"), old);
    prerender(
        &site,
        r#""/articles/{slug}/{page: Int}" = ["/articles/a/1"]"#,
    );
    success(site.build());
    assert!(!site.0.join("dist/articles/z").exists());
}

#[test]
fn invalid_enumeration_preserves_the_last_good_site() {
    let site = Site::new();
    site.source(STATIC);
    success(site.build());
    let old = site.read("dist/index.html");
    site.source(PARAMETERIZED);
    for (entries, diagnostic) in [
        (
            r#""/articles/{slug}/{page: Int}" = ["/articles/a/nope"]"#,
            "expected Int",
        ),
        (
            r#""/articles/{slug}/{page: Int}" = ["/articles/../2"]"#,
            "invalid static route path",
        ),
        (
            r#""/articles/{slug}/{page: Int}" = ["/other/a/1"]"#,
            "does not match",
        ),
        (r#""/missing/{slug}" = ["/missing/a"]"#, "registered GET"),
        (r#""/articles/{slug}/{page: Int}" = []"#, "empty"),
        (
            r#""/articles/{slug}/{page: Int}" = ["/articles/a/1", "/articles/A/1"]"#,
            "conflicts",
        ),
    ] {
        prerender(&site, entries);
        let output = site.build();
        assert!(!output.status.success(), "{entries}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "{entries}: {stderr}");
        assert_eq!(site.read("dist/index.html"), old);
    }
}

#[test]
fn finite_route_pages_match_http_and_static_host_redirects() {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};
    let site = Site::new();
    site.source(PARAMETERIZED);
    prerender(
        &site,
        r#""/articles/{slug}/{page: Int}" = ["/articles/a/1"]"#,
    );
    success(site.build());
    let ir = site.ir();
    let route = &ir.routes[0];
    assert_eq!(
        route.route_template.as_deref(),
        Some("/articles/{slug}/{page: Int}")
    );
    assert!(
        route.params.is_empty() && route.handler_params.is_empty() && route.bindings.is_empty()
    );
    nulang::web::adapters::static_host::generate_files(&site.0.join("dist"), &ir).unwrap();
    let redirects = String::from_utf8(site.read("dist/_redirects")).unwrap();
    assert!(redirects.contains("/articles/a/1 articles/a/1/index.html 200"));
    assert!(!redirects.contains("{slug}"));

    let artifact = nulang::bytecode::CodeModule::from_nbc(&site.read(".nula/build.nbc")).unwrap();
    let mut vm = nulang::vm::VM::new();
    vm.load_module(artifact.module);
    vm.run().unwrap();
    let routes =
        nulang::web::dispatch::compile_runtime_routes(vm.take_web_routes(), &site.0.join("src"))
            .unwrap();
    let server = nulang::runtime::WebDevServer::bind_runtime(0, None, None, routes).unwrap();
    for (url, body) in [
        ("/articles/a/1", site.read("dist/articles/a/1/index.html")),
        ("/articles/unlisted/3", b"unlisted:3".to_vec()),
    ] {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {url} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(response.split_once("\r\n\r\n").unwrap().1.as_bytes(), body);
    }
    assert!(!site.0.join("dist/articles/unlisted").exists());
}

#[test]
fn finite_route_rejects_ambient_effects_and_request_bindings() {
    let site = Site::new();
    prerender(&site, r#""/article/{slug}" = ["/article/intro"]"#);
    for (handler, diagnostic) in [
        (
            "fn article(slug: String) -> Html { text(perform Web.cookie(slug)) }",
            "disallows Web.cookie",
        ),
        (
            "fn article(slug: String, session: String from cookie) -> Html { text(session) }",
            "path-only",
        ),
        (
            "@placement(server)\nfn article(slug: String) -> Html { text(slug) }",
            "explicit placement",
        ),
        (
            "fn article(slug) -> Html { text(slug) }",
            "explicitly typed",
        ),
    ] {
        site.source(&format!(
            r#"
import stdlib::web::html
import stdlib::web::types
{handler}
app "site" {{
    route "GET" "/article/{{slug}}" -> article
}}
"#
        ));
        let output = site.build();
        assert!(!output.status.success(), "{handler}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "{handler}: {stderr}");
        assert!(!site.0.join("dist").exists());
    }
}

#[test]
fn finite_route_cannot_shadow_any_other_get_handler() {
    let site = Site::new();
    prerender(&site, r#""/article/{slug}" = ["/article/intro"]"#);
    for other in ["/article/intro", "/article/INTRO", "/{section}/{name}"] {
        site.source(&format!(
            r#"
import stdlib::web::html
import stdlib::web::types
fn article(slug: String) -> Html {{ text(slug) }}
fn other() -> Html {{ text("literal") }}
fn dynamic(section: String, name: String) -> Html {{ text(name) }}
app "site" {{
    route "GET" "/article/{{slug}}" -> article
    route "GET" "{other}" -> {}
}}
"#,
            if other.contains('{') {
                "dynamic"
            } else {
                "other"
            }
        ));
        let output = site.build();
        assert!(!output.status.success(), "{other}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("conflicts"), "{other}: {stderr}");
        assert!(!site.0.join("dist").exists());
    }
}

#[test]
fn finite_routes_accept_explicit_static_placement_and_scalar_path_bindings() {
    let site = Site::new();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
@placement(static)
fn flag(value: Bool from path("flag"), amount: Float) -> Html {
    if value then text(perform Float.to_string(amount)) else text("off")
}
fn other(slug: String) -> Html { text(slug) }
app "site" {
    route "GET" "/items/{flag: Bool}/{amount: Float}" -> flag
    route "GET" "/other/{slug}" -> other
}
"#,
    );
    prerender(
        &site,
        r#""/items/{flag: Bool}/{amount: Float}" = ["/items/true/2.5", "/items/false/1.5"]"#,
    );
    success(site.build());
    assert_eq!(site.read("dist/items/true/2.5/index.html"), b"2.5");
    assert_eq!(site.read("dist/items/false/1.5/index.html"), b"off");
    let ir = site.ir();
    assert_eq!(ir.routes.len(), 3);
    assert_eq!(ir.routes[2].path, "/other/{slug}");
    assert_eq!(ir.routes[2].placement, "server");
    assert!(ir.routes[2].artifact.is_none() && ir.routes[2].route_template.is_none());
}
