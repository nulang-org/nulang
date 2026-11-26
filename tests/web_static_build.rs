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

#[test]
fn cold_request_effect_through_helper_prevents_static_publication() {
    let site = Site::new();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
fn leaf(flag: Bool) -> Html {
    if flag then text(perform Web.cookie("session")) else text("preview")
}
fn middle(flag: Bool) -> Html { leaf(flag) }
fn home() -> Html { middle(false) }
fn pure() -> Html { text("independent") }
app "site" {
    route "GET" "/" -> home
    route "GET" "/pure" -> pure
}
"#,
    );
    let output = site.build();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!site.0.join("dist/index.html").exists());
    let ir = site.ir();
    assert_eq!(ir.routes[0].placement, "server");
    assert!(ir.routes[0].artifact.is_none());
    assert_eq!(site.read("dist/pure/index.html"), b"independent");
}

#[test]
fn finite_prerender_rejects_effect_on_unlisted_branch_and_preserves_site() {
    let site = Site::new();
    site.source(STATIC);
    success(site.build());
    let old = site.read("dist/index.html");
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
fn leaf(slug: String) -> Html {
    if slug == "private" then text(perform Web.cookie("session")) else text(slug)
}
fn article(slug: String) -> Html { leaf(slug) }
app "site" {
    route "GET" "/articles/{slug}" -> article
}
"#,
    );
    prerender(&site, r#""/articles/{slug}" = ["/articles/public"]"#);
    let output = site.build();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("disallows Web.cookie"), "{stderr}");
    assert_eq!(site.read("dist/index.html"), old);
    assert!(!site.0.join("dist/articles").exists());
}

#[test]
fn imported_callee_effects_are_proved_on_linked_bytecode_only_for_called_functions() {
    let site = Site::new();
    std::fs::write(
        site.0.join("src/details.nula"),
        r#"
fn private_detail(flag: Bool) -> String {
    if flag then perform Web.cookie("session") else "preview"
}
fn public_detail() -> String { "public" }
"#,
    )
    .unwrap();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
import details
fn private_page() -> Html { text(private_detail(false)) }
fn public_page() -> Html { text(public_detail()) }
app "site" {
    route "GET" "/private" -> private_page
    route "GET" "/public" -> public_page
}
"#,
    );
    success(site.build());
    assert!(!site.0.join("dist/private/index.html").exists());
    assert_eq!(site.read("dist/public/index.html"), b"public");
    let ir = site.ir();
    assert_eq!(ir.routes[0].placement, "server");
    assert_eq!(ir.routes[1].placement, "static");
}

#[test]
fn direct_recursive_renderer_is_not_disqualified_merely_for_recursion() {
    let site = Site::new();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
fn countdown(n: Int) -> String {
    if n == 0 then "ok" else countdown(n - 1)
}
fn home() -> Html { text(countdown(3)) }
app "site" {
    route "GET" "/" -> home
}
"#,
    );
    success(site.build());
    assert_eq!(site.read("dist/index.html"), b"ok");
    assert_eq!(site.ir().routes[0].placement, "static");
}

fn collections(site: &Site, entries: &str) {
    let path = site.0.join("Nulang.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    let prefix = original.split("[web.collections]").next().unwrap();
    std::fs::write(path, format!("{prefix}\n[web.collections]\n{entries}\n")).unwrap();
}

const COLLECTION_APP: &str = r#"
import stdlib::web::html
import stdlib::web::types
import articles

fn article_keys() -> [String] {
    var keys = [];
    for item in articles() {
        keys = perform Array.push(keys, item.slug)
    }
    keys
}
fn article(slug: String) -> Html {
    var title = "not found";
    for item in articles() {
        if item.slug == slug then { title = item.title } else {}
    }
    text(title)
}
app "collection" {
    route "GET" "/articles/{slug}" -> article
}
"#;

#[test]
fn native_typed_collection_drives_pages_and_removes_stale_artifacts() {
    let site = Site::new();
    site.source(COLLECTION_APP);
    let data = site.0.join("src/articles.nula");
    let write_items = |items: &str| {
        std::fs::write(&data, format!(
            "pub type Article = {{ slug: String, title: String }}\npub fn articles() -> [Article] {{ [{items}] }}\n"
        )).unwrap();
    };
    write_items(r#"{slug: "z", title: "Last"}, {slug: "intro", title: "Introduction"}"#);
    collections(&site, r#""/articles/{slug}" = "article_keys""#);
    success(site.build());
    assert_eq!(site.read("dist/articles/intro/index.html"), b"Introduction");
    assert_eq!(site.read("dist/articles/z/index.html"), b"Last");
    let ir = site.ir();
    assert_eq!(ir.routes.len(), 2);
    assert_eq!(ir.routes[0].path, "/articles/intro");
    assert_eq!(ir.routes[1].path, "/articles/z");
    assert!(ir.routes.iter().all(|r| r.placement == "static"
        && r.route_template.as_deref() == Some("/articles/{slug}")
        && r.bindings.is_empty()
        && r.artifact.is_some()));
    let old_ir = site.read("dist/nulang-app.ir.json");
    write_items(r#"{slug: "intro", title: "Introduction"}, {slug: "z", title: "Last"}"#);
    success(site.build());
    assert_eq!(site.read("dist/nulang-app.ir.json"), old_ir);

    // Use the same compiler-owned dispatch plan, not a separate SSG renderer.
    let artifact = nulang::bytecode::CodeModule::from_nbc(&site.read(".nula/build.nbc")).unwrap();
    let mut vm = nulang::vm::VM::new();
    vm.load_module(artifact.module);
    vm.run().unwrap();
    let routes =
        nulang::web::dispatch::compile_runtime_routes(vm.take_web_routes(), &site.0.join("src"))
            .unwrap();
    let params = nulang::web::dispatch::match_route(&routes[0], "/articles/intro").unwrap();
    let rendered = nulang::web::dispatch::render_direct_route(&routes[0], &params)
        .unwrap()
        .unwrap();
    assert_eq!(
        rendered.as_bytes(),
        site.read("dist/articles/intro/index.html")
    );
    write_items(r#"{slug: "intro", title: "Revised"}"#);
    success(site.build());
    assert_eq!(site.read("dist/articles/intro/index.html"), b"Revised");
    assert!(!site.0.join("dist/articles/z").exists());
}

#[test]
fn collection_tuple_keys_follow_url_order_not_handler_order() {
    let site = Site::new();
    site.source(r#"
import stdlib::web::html
import stdlib::web::types
fn keys() -> [(String, Int, Bool, Float)] { [("a", 2, true, 1.5)] }
fn item(amount: Float, page: Int, slug: String, flag: Bool) -> Html {
    if flag then text(slug + ":" + perform Int.to_string(page) + ":" + perform Float.to_string(amount)) else text("off")
}
fn other(slug: String) -> Html { text(slug) }
app "site" {
    route "GET" "/items/{slug}/{page: Int}/{flag: Bool}/{amount: Float}" -> item
    route "GET" "/other/{slug}" -> other
    route "GET" "/runtime/{slug}" -> other
}
"#);
    prerender(&site, r#""/other/{slug}" = ["/other/manual"]"#);
    collections(
        &site,
        r#""/items/{slug}/{page: Int}/{flag: Bool}/{amount: Float}" = "keys""#,
    );
    success(site.build());
    assert_eq!(site.read("dist/items/a/2/true/1.5/index.html"), b"a:2:1.5");
    assert_eq!(site.read("dist/other/manual/index.html"), b"manual");
    assert_eq!(site.ir().routes[2].placement, "server");
}

#[test]
fn invalid_collections_fail_closed_and_preserve_last_good_site() {
    let site = Site::new();
    site.source(STATIC);
    success(site.build());
    let old = site.read("dist/index.html");
    for (provider, diagnostic) in [
        ("fn keys(x: String) -> [String] { [x] }", "zero arguments"),
        ("fn keys() -> String { \"a\" }", "must return an array"),
        ("fn keys() -> [String] { [] }", "empty collection"),
        ("fn keys() -> [Int] { [1] }", "must be String"),
        ("fn keys() -> [String] { [\"a\", \"A\"] }", "conflicts"),
        (
            "fn keys() -> [String] { [\"..\"] }",
            "invalid static route path",
        ),
        (
            "fn keys() -> [String] { [\"a/b\"] }",
            "invalid static route path key",
        ),
        (
            "fn keys() -> [String] { [\"\"] }",
            "invalid static route path key",
        ),
        (
            "fn keys() -> [(String, Int)] { [(\"a\", 1)] }",
            "must be String",
        ),
        (
            "fn differently_named() -> [String] { [\"a\"] }",
            "compiled provider function",
        ),
        (
            r#"fn leaf(flag: Bool) -> [String] {
            if flag then [perform Web.cookie("session")] else ["a"]
        }
        fn keys() -> [String] { leaf(false) }"#,
            "Web.cookie",
        ),
    ] {
        site.source(&format!(
            r#"
import stdlib::web::html
import stdlib::web::types
{provider}
fn article(slug: String) -> Html {{ text(slug) }}
app "site" {{ route "GET" "/articles/{{slug}}" -> article }}
"#
        ));
        collections(&site, r#""/articles/{slug}" = "keys""#);
        let output = site.build();
        assert!(!output.status.success(), "{provider}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "{provider}: {stderr}");
        assert_eq!(site.read("dist/index.html"), old);
    }
}

#[test]
fn collection_and_manual_enumeration_cannot_configure_same_route() {
    let site = Site::new();
    site.source(PARAMETERIZED);
    prerender(
        &site,
        r#""/articles/{slug}/{page: Int}" = ["/articles/a/1"]"#,
    );
    collections(&site, r#""/articles/{slug}/{page: Int}" = "keys""#);
    let output = site.build();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("configured in both"));
    assert!(!site.0.join("dist").exists());
}

#[test]
fn invalid_collection_configuration_and_tuple_shape_are_rejected() {
    let site = Site::new();
    for (keys, entries, placement, diagnostic) in [
        (
            "fn keys() -> [(String, Int, Bool)] { [(\"a\", 1, true)] }",
            r#""/articles/{slug}/{page: Int}" = "keys""#,
            "",
            "expected a tuple of 2 path keys",
        ),
        (
            "fn keys() -> [(Int, String)] { [(1, \"a\")] }",
            r#""/articles/{slug}/{page: Int}" = "keys""#,
            "",
            "path key 'slug' must be String",
        ),
        (
            "fn keys() -> [(String, Int)] { [(\"a\", 1)] }",
            r#""/missing/{slug}" = "keys""#,
            "",
            "registered GET",
        ),
        (
            "fn keys() -> [(String, Int)] { [(\"a\", 1)] }",
            r#""/articles/{slug}/{page: Int}" = "keys""#,
            "@placement(server)",
            "conflicts with explicit placement",
        ),
    ] {
        site.source(&format!(
            r#"
import stdlib::web::html
import stdlib::web::types
{keys}
{placement}
fn article(slug: String, page: Int) -> Html {{ text(slug) }}
app "site" {{ route "GET" "/articles/{{slug}}/{{page: Int}}" -> article }}
"#
        ));
        collections(&site, entries);
        let output = site.build();
        assert!(!output.status.success(), "{keys}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "{keys}: {stderr}");
        assert!(!site.0.join("dist").exists());
    }
}

const MARKDOWN_APP: &str = r#"
import stdlib::web::html
import stdlib::web::types
import @nulang/web_content

fn article_keys() -> [String] {
    var keys = [];
    for item in content_articles() {
        keys = perform Array.push(keys, item.slug)
    }
    keys
}
fn article(slug: String) -> Html {
    var page = "not found";
    for item in content_articles() {
        if item.slug == slug then {
            page = item.title + "|" + item.summary + "|" + item.body + "|" + item.source
        } else {}
    }
    text(page)
}
app "content site" {
    route "GET" "/articles/{slug}" -> article
}
"#;

fn markdown_config(site: &Site, content: &str) {
    let manifest = site.0.join("Nulang.toml");
    let original = std::fs::read_to_string(&manifest).unwrap();
    let prefix = original.split("[web.content]").next().unwrap();
    std::fs::write(manifest, format!("{prefix}\n[web.content]\n{content}\n")).unwrap();
}

fn markdown_file(site: &Site, path: &str, contents: &str) {
    let path = site.0.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn content_site() -> Site {
    let site = Site::new();
    site.source(MARKDOWN_APP);
    collections(&site, r#""/articles/{slug}" = "article_keys""#);
    markdown_config(&site, r#"articles = "content/articles""#);
    site
}

#[test]
fn markdown_bundles_are_typed_deterministic_and_draft_free() {
    let site = content_site();
    markdown_file(
        &site,
        "content/articles/z.md",
        "+++\nslug = \"last\"\ntitle = \"Last\"\n+++\nLast body\n",
    );
    markdown_file(&site, "content/articles/intro/index.md", "+++\r\nslug = \"intro\"\r\ntitle = 'Introdução \"quoted\"'\r\nsummary = \"Summary\"\r\ntags = [\"docs\", \"start\"]\r\n+++\r\n# Heading\r\n<script>unsafe</script>\r\n\\ literal\r\n");
    markdown_file(
        &site,
        "content/articles/draft.md",
        "+++\nslug = \"hidden\"\ntitle = \"Not published\"\ndraft = true\n+++\nDRAFT_SENTINEL\n",
    );
    markdown_file(
        &site,
        "content/articles/intro/attachment.txt",
        "not a Markdown entry",
    );
    success(site.build());
    let intro = String::from_utf8(site.read("dist/articles/intro/index.html")).unwrap();
    assert!(intro.contains("Introdução"), "{intro}");
    assert!(intro.contains("Summary|# Heading\n"), "{intro}");
    assert!(
        intro.contains("&lt;script&gt;unsafe&lt;/script&gt;"),
        "{intro}"
    );
    assert!(
        intro.contains("\\ literal\n|content/articles/intro/index.md"),
        "{intro}"
    );
    assert!(!site.0.join("dist/articles/hidden").exists());
    let nbc = site.read(".nula/build.nbc");
    assert!(!nbc
        .windows(b"DRAFT_SENTINEL".len())
        .any(|w| w == b"DRAFT_SENTINEL"));
    assert_eq!(site.ir().routes.len(), 2);
    let ir = site.read("dist/nulang-app.ir.json");
    success(site.build());
    assert_eq!(site.read("dist/nulang-app.ir.json"), ir);
    assert_eq!(
        site.read("dist/articles/intro/index.html"),
        intro.as_bytes()
    );

    // Dispatch the exact same compiled renderer from the validated snapshot.
    let artifact = nulang::bytecode::CodeModule::from_nbc(&site.read(".nula/build.nbc")).unwrap();
    let mut vm = nulang::vm::VM::new();
    vm.load_module(artifact.module);
    vm.run().unwrap();
    let routes =
        nulang::web::dispatch::compile_runtime_routes(vm.take_web_routes(), &site.0.join("src"))
            .unwrap();
    let params = nulang::web::dispatch::match_route(&routes[0], "/articles/intro").unwrap();
    assert_eq!(
        nulang::web::dispatch::render_direct_route(&routes[0], &params)
            .unwrap()
            .unwrap(),
        intro,
    );
    std::fs::remove_file(site.0.join("content/articles/z.md")).unwrap();
    success(site.build());
    assert!(!site.0.join("dist/articles/last").exists());
    assert_eq!(site.ir().routes.len(), 1);
    assert_eq!(
        std::fs::read_dir(site.0.join(".nula"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("web-content-"))
            .count(),
        0
    );
}

#[test]
fn invalid_markdown_metadata_preserves_last_good_site() {
    let site = content_site();
    markdown_file(
        &site,
        "content/articles/a.md",
        "+++\nslug = \"a\"\ntitle = \"A\"\n+++\nStable\n",
    );
    success(site.build());
    let old = site.read("dist/articles/a/index.html");
    let bad = "content/articles/b.md";
    for (document, diagnostic) in [
        ("# missing metadata", "expected TOML front matter"),
        ("+++\nslug = \"b\"\ntitle = \"B\"\n", "missing closing"),
        ("+++\nslug = \"b\"\n+++\n", "invalid front matter"),
        (
            "+++\nslug = \"b\"\ntitle = 42\n+++\n",
            "invalid front matter",
        ),
        (
            "+++\nslug = \"b\"\ntitle = \"B\"\ntypo = true\n+++\n",
            "invalid front matter",
        ),
        (
            "+++\nslug = \"b\"\ntitle = \" \"\n+++\n",
            "title must not be empty",
        ),
        (
            "+++\nslug = \"b\"\ntitle = \"B\"\ntags = [\"a\", \"a\"]\n+++\n",
            "tags must be",
        ),
        (
            "+++\nslug = \"../b\"\ntitle = \"B\"\n+++\n",
            "portable nonempty",
        ),
        (
            "+++\nslug = \"A\"\ntitle = \"B\"\ndraft = true\n+++\n",
            "duplicate content slug",
        ),
        (
            "+++\nslug = \"b\"\ntitle = \"B\"\n+++\n\0",
            "must not contain NUL",
        ),
    ] {
        markdown_file(&site, bad, document);
        let output = site.build();
        assert!(!output.status.success(), "{document}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(diagnostic) && stderr.contains("content/articles/b.md"),
            "{diagnostic}: {stderr}"
        );
        assert_eq!(site.read("dist/articles/a/index.html"), old);
    }
}

#[test]
fn content_directories_cannot_overlap_output_or_assets_or_escape_root() {
    let site = content_site();
    for (config, diagnostic) in [
        (
            r#"articles = "../outside""#,
            "relative path inside the package",
        ),
        (r#"articles = "dist/articles""#, "overlaps protected path"),
        (r#"articles = "public/articles""#, "overlaps protected path"),
        (r#"articles = ".nula/articles""#, "overlaps protected path"),
        (
            r#""bad-name" = "content/articles""#,
            "invalid web content bundle name",
        ),
    ] {
        markdown_config(&site, config);
        let output = site.build();
        assert!(!output.status.success(), "{config}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "{diagnostic}: {stderr}");
        assert!(!site.0.join("dist").exists());
    }
}

#[cfg(unix)]
#[test]
fn content_symlinks_are_rejected_without_following_them() {
    use std::os::unix::fs::symlink;
    let site = content_site();
    markdown_file(
        &site,
        "outside.md",
        "+++\nslug = \"secret\"\ntitle = \"Secret\"\n+++\nPrivate\n",
    );
    std::fs::create_dir_all(site.0.join("content/articles")).unwrap();
    symlink(
        site.0.join("outside.md"),
        site.0.join("content/articles/link.md"),
    )
    .unwrap();
    let output = site.build();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("only real files and directories"));
    assert!(!site.0.join("dist").exists());
}

#[test]
fn content_artifacts_are_byte_identical_across_clean_packages() {
    fn snapshot(path: &Path, root: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                snapshot(&path, root, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().into(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let a = content_site();
    let b = content_site();
    let intro = "+++\nslug = \"intro\"\ntitle = \"Intro\"\n+++\n# Heading\n";
    let last = "+++\nslug = \"last\"\ntitle = \"Last\"\n+++\nLast\n";
    for (path, doc) in [
        ("content/articles/a/index.md", intro),
        ("content/articles/z.md", last),
    ] {
        markdown_file(&a, path, doc);
    }
    for (path, doc) in [
        ("content/articles/z.md", last),
        ("content/articles/a/index.md", intro),
    ] {
        markdown_file(&b, path, doc);
    }
    success(a.build());
    success(b.build());
    let mut first = std::collections::BTreeMap::new();
    let mut second = std::collections::BTreeMap::new();
    snapshot(&a.0.join("dist"), &a.0.join("dist"), &mut first);
    snapshot(&b.0.join("dist"), &b.0.join("dist"), &mut second);
    assert_eq!(first, second);
}

#[cfg(unix)]
#[test]
fn content_output_alias_is_rejected() {
    use std::os::unix::fs::symlink;
    let site = content_site();
    markdown_file(
        &site,
        "content/articles/a.md",
        "+++\nslug = \"a\"\ntitle = \"A\"\n+++\n",
    );
    let original = site.read("content/articles/a.md");
    symlink(site.0.join("content/articles"), site.0.join("dist")).unwrap();
    let output = site.build();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("web content/output path must be a real directory"));
    assert_eq!(site.read("content/articles/a.md"), original);
}

#[cfg(feature = "ureq")]
#[test]
fn content_deploy_dryrun_reuses_snapshot_for_runtime_artifact() {
    let site = content_site();
    markdown_file(
        &site,
        "content/articles/a.md",
        "+++\nslug = \"a\"\ntitle = \"A\"\n+++\nCompiled body\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .current_dir(&site.0)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .env_remove("CARGO_BIN_EXE_nulang")
        .env("PATH", "")
        .args(["nula", "deploy", "--dry-run", "--adapter", "static"])
        .output()
        .unwrap();
    success(output);
    let artifact =
        nulang::bytecode::CodeModule::from_nbc(&site.read(".nula/dist/static-fixture.nbc"))
            .unwrap();
    let mut vm = nulang::vm::VM::new();
    vm.load_module(artifact.module);
    vm.run().unwrap();
    let routes =
        nulang::web::dispatch::compile_runtime_routes(vm.take_web_routes(), &site.0.join("src"))
            .unwrap();
    let params = nulang::web::dispatch::match_route(&routes[0], "/articles/a").unwrap();
    let body = nulang::web::dispatch::render_direct_route(&routes[0], &params)
        .unwrap()
        .unwrap();
    assert_eq!(body.as_bytes(), site.read("dist/articles/a/index.html"));
}

#[test]
fn dev_server_uses_content_snapshot_and_does_not_publish_drafts() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::process::Stdio;
    use std::time::Duration;

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let site = content_site();
    markdown_file(
        &site,
        "content/articles/a.md",
        "+++\nslug = \"a\"\ntitle = \"Published\"\n+++\nBody\n",
    );
    markdown_file(
        &site,
        "content/articles/draft.md",
        "+++\nslug = \"draft\"\ntitle = \"Private\"\ndraft = true\n+++\nPrivate body\n",
    );
    success(site.build());
    let expected = site.read("dist/articles/a/index.html");
    let stderr = std::fs::File::create(site.0.join("dev.stderr")).unwrap();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_nulang"))
            .current_dir(&site.0)
            .env(
                "NULANG_STDLIB",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
            )
            .env_remove("CARGO_BIN_EXE_nulang")
            .env("PATH", "")
            .args(["nula", "dev", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(address) = line.strip_prefix("Dev server listening on http://") {
                let _ = send.send(address.trim_end_matches('/').to_owned());
                break;
            }
        }
    });
    let address = receive
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|err| {
            panic!(
                "dev startup: {err}: {}",
                String::from_utf8_lossy(&site.read("dev.stderr"))
            )
        });
    reader.join().unwrap();
    let get = |path: &str| {
        let mut stream = TcpStream::connect(&address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.split_once("\r\n\r\n").unwrap().1.to_owned()
    };
    assert_eq!(get("/articles/a").as_bytes(), expected);
    assert_eq!(get("/articles/draft"), "not found");
    // The dev runtime no longer needs a snapshot source on disk.
    assert!(!std::fs::read_dir(site.0.join(".nula"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().starts_with("web-content-")));
}

const MARKDOWN_HTML_APP: &str = r#"
import stdlib::web::html
import stdlib::web::types
import @nulang/web_content

fn article_keys() -> [String] {
    var keys = [];
    for item in content_articles() {
        keys = perform Array.push(keys, item.slug)
    }
    keys
}
fn article(slug: String) -> Html {
    var page = text("not found");
    for item in content_articles() {
        if item.slug == slug then { page = item.html } else {}
    }
    page
}
fn preview(slug: String) -> Html { text(slug) }
app "content site" {
    route "GET" "/articles/{slug}" -> article
    route "GET" "/preview/{slug}" -> preview
}
"#;

#[test]
fn commonmark_content_renders_typed_html_with_validated_links() {
    let site = content_site();
    site.source(MARKDOWN_HTML_APP);
    markdown_file(
        &site, "content/articles/a.md",
        "+++\nslug = \"a\"\ntitle = \"A\"\n+++\n# Heading\n\n**Bold** *emphasis*\n\n- item\n\n[B](/articles/b/?a=1&b=2#top) ![Photo](/images/caf%C3%A9.png) [Preview](/preview/demo)\n\n<iframe src=x></iframe>\n\nHello <img src=x onerror=alert(1)>\n",
    );
    markdown_file(
        &site,
        "content/articles/b.md",
        "+++\nslug = \"b\"\ntitle = \"B\"\n+++\nAnother page\n",
    );
    markdown_file(&site, "public/images/café.png", "test image");
    success(site.build());
    let html = String::from_utf8(site.read("dist/articles/a/index.html")).unwrap();
    for fragment in [
        "<h1>Heading</h1>",
        "<strong>Bold</strong>",
        "<em>emphasis</em>",
        "<li>item</li>",
        "href=\"/articles/b/?a=1&amp;b=2#top\"",
        "src=\"/images/caf%C3%A9.png\" alt=\"Photo\"",
        "href=\"/preview/demo\"",
        "&lt;iframe src=x&gt;&lt;/iframe&gt;",
        "&lt;img src=x onerror=alert(1)&gt;",
    ] {
        assert!(html.contains(fragment), "expected {fragment}: {html}");
    }
    assert!(!html.contains("<iframe"));
    assert!(!html.contains("<img src=x"));

    // Compiled HTTP handlers return the identical typed fragment.
    let artifact = nulang::bytecode::CodeModule::from_nbc(&site.read(".nula/build.nbc")).unwrap();
    let mut vm = nulang::vm::VM::new();
    vm.load_module(artifact.module);
    vm.run().unwrap();
    let routes =
        nulang::web::dispatch::compile_runtime_routes(vm.take_web_routes(), &site.0.join("src"))
            .unwrap();
    let route = routes
        .iter()
        .find(|route| route.route.path == "/articles/{slug}")
        .unwrap();
    let params = nulang::web::dispatch::match_route(route, "/articles/a").unwrap();
    assert_eq!(
        nulang::web::dispatch::render_direct_route(route, &params)
            .unwrap()
            .unwrap(),
        html
    );
}

#[test]
fn unsafe_markdown_and_broken_root_links_fail_without_publishing() {
    let site = content_site();
    site.source(MARKDOWN_HTML_APP);
    markdown_file(
        &site,
        "content/articles/a.md",
        "+++\nslug = \"a\"\ntitle = \"A\"\n+++\nStable\n",
    );
    markdown_file(
        &site,
        "content/articles/draft.md",
        "+++\nslug = \"hidden\"\ntitle = \"Hidden\"\ndraft = true\n+++\n[unsafe](javascript:run)\n",
    );
    success(site.build());
    let old = site.read("dist/articles/a/index.html");
    for (body, diagnostic) in [
        ("[x](JaVaScRiPt&colon;alert%281%29)", "unsafe Markdown URL"),
        ("![x](data:image/svg+xml;base64,xyz)", "unsafe Markdown URL"),
        ("[x](/articles/hidden)", "broken root-relative Markdown URL"),
        (
            "[x](/articles/missing)",
            "broken root-relative Markdown URL",
        ),
        ("![x](/images/no.png)", "broken root-relative Markdown URL"),
        ("[x](/%2e%2e/secret)", "invalid root-relative Markdown URL"),
        (
            "[x](/images/%2fsecret)",
            "invalid root-relative Markdown URL",
        ),
    ] {
        markdown_file(
            &site,
            "content/articles/a.md",
            &format!("+++\nslug = \"a\"\ntitle = \"A\"\n+++\n{body}\n"),
        );
        let result = site.build();
        assert!(!result.status.success(), "{body}");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains(diagnostic) && stderr.contains("content/articles/a.md"),
            "{body}: {stderr}"
        );
        assert_eq!(site.read("dist/articles/a/index.html"), old);
    }
}

#[test]
fn root_links_validate_against_new_not_previous_artifacts() {
    let site = content_site();
    site.source(MARKDOWN_HTML_APP);
    markdown_file(
        &site,
        "content/articles/a.md",
        "+++\nslug = \"a\"\ntitle = \"A\"\n+++\n[B](/articles/b/)\n",
    );
    markdown_file(
        &site,
        "content/articles/b.md",
        "+++\nslug = \"b\"\ntitle = \"B\"\n+++\nB\n",
    );
    success(site.build());
    let old = site.read("dist/articles/a/index.html");
    std::fs::remove_file(site.0.join("content/articles/b.md")).unwrap();
    let result = site.build();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("broken root-relative Markdown URL"));
    assert_eq!(site.read("dist/articles/a/index.html"), old);
    assert!(site.0.join("dist/articles/b/index.html").exists());
}

#[test]
fn sitemap_uses_only_concrete_static_routes_in_sorted_order() {
    let site = Site::new();
    let manifest = site.0.join("Nulang.toml");
    let mut config = std::fs::read_to_string(&manifest).unwrap();
    config.push_str("site_url = \"https://example.com/\"\n[web.prerender]\n\"/posts/{slug}\" = [\"/posts/z\", \"/posts/a\"]\n");
    std::fs::write(&manifest, config).unwrap();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
fn home() -> Html { text("home") }
fn post(slug: String) -> Html { text(slug) }
@placement(server)
fn account() -> Html { text("private") }
app "sitemap" {
    route "GET" "/posts/{slug}" -> post
    route "GET" "/" -> home
    route "GET" "/account" -> account
}
"#,
    );
    success(site.build());
    let expected = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
        "  <url><loc>https://example.com/</loc></url>\n",
        "  <url><loc>https://example.com/posts/a</loc></url>\n",
        "  <url><loc>https://example.com/posts/z</loc></url>\n",
        "</urlset>\n"
    );
    assert_eq!(site.read("dist/sitemap.xml"), expected.as_bytes());
    success(site.build());
    assert_eq!(site.read("dist/sitemap.xml"), expected.as_bytes());
}

#[test]
fn sitemap_collision_and_invalid_origin_preserve_previous_site() {
    let site = Site::new();
    site.source(STATIC);
    let manifest = site.0.join("Nulang.toml");
    let original = std::fs::read_to_string(&manifest).unwrap();
    let valid = format!("{original}site_url = \"https://example.com\"\n");
    std::fs::write(&manifest, &valid).unwrap();
    success(site.build());
    let previous = site.read("dist/sitemap.xml");
    for invalid in [
        "https://user@example.com",
        "https://example.com/path",
        "https://example.com?x",
        "javascript:alert(1)",
    ] {
        std::fs::write(&manifest, format!("{original}site_url = {invalid:?}\n")).unwrap();
        assert!(!site.build().status.success(), "{invalid}");
        assert_eq!(site.read("dist/sitemap.xml"), previous);
    }
    std::fs::write(&manifest, &valid).unwrap();
    std::fs::write(site.0.join("public/SITEMAP.XML"), "collision").unwrap();
    assert!(!site.build().status.success());
    assert_eq!(site.read("dist/sitemap.xml"), previous);
    std::fs::remove_file(site.0.join("public/SITEMAP.XML")).unwrap();
    std::fs::write(&manifest, original).unwrap();
    success(site.build());
    assert!(!site.0.join("dist/sitemap.xml").exists());
}

#[test]
fn sitemap_rejects_a_claimed_url_and_an_empty_static_site() {
    let site = Site::new();
    let manifest = site.0.join("Nulang.toml");
    let mut config = std::fs::read_to_string(&manifest).unwrap();
    config.push_str("site_url = \"https://example.com\"\n");
    std::fs::write(&manifest, config).unwrap();
    site.source(STATIC);
    success(site.build());
    let previous = site.read("dist/sitemap.xml");
    for source in [
        r#"
import stdlib::web::html
import stdlib::web::types
@placement(server)
fn account() -> Html { text("server") }
app "site" { route "GET" "/" -> account }
"#,
        r#"
import stdlib::web::html
import stdlib::web::types
fn home() -> Html { text("home") }
@placement(server)
fn intercept() -> Html { text("server") }
app "site" {
    route "GET" "/" -> home
    route "GET" "/sitemap.xml" -> intercept
}
"#,
        r#"
import stdlib::web::html
import stdlib::web::types
fn home() -> Html { text("home") }
app "site" {
    route "GET" "/" -> home
    route "GET" "/SITEMAP.XML" -> home
}
"#,
    ] {
        site.source(source);
        let output = site.build();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("sitemap")
                || String::from_utf8_lossy(&output.stderr).contains("web.site_url")
        );
        assert_eq!(site.read("dist/sitemap.xml"), previous);
    }
}

#[test]
fn test_server_route_aliases_do_not_require_static_artifacts() {
    let site = Site::new();
    site.source(
        r#"
import stdlib::web::html
import stdlib::web::types
@placement(server)
fn server() -> Html { text("server") }
fn inferred() -> Html { text(perform Web.cookie("session")) }
app "server-aliases" {
    route "GET" "/docs" -> server
    route "GET" "/docs/" -> server
    route "GET" "/Account" -> inferred
    route "GET" "/account" -> inferred
}
"#,
    );
    success(site.build());
    let ir = site.ir();
    assert_eq!(ir.routes.len(), 4);
    assert!(ir
        .routes
        .iter()
        .all(|r| r.placement == "server" && r.artifact.is_none()));
    assert!(!site.0.join("dist/docs").exists());
    assert!(!site.0.join("dist/account").exists());
    assert!(!site.0.join("dist/Account").exists());
}

#[test]
fn test_static_page_cannot_shadow_literal_server_alias_in_either_order() {
    let site = Site::new();
    site.source(STATIC);
    success(site.build());
    let previous = site.read("dist/index.html");
    for routes in [
        "route \"GET\" \"/docs\" -> page\nroute \"GET\" \"/docs/\" -> server",
        "route \"GET\" \"/docs/\" -> server\nroute \"GET\" \"/docs\" -> page",
        "route \"GET\" \"/Account\" -> page\nroute \"GET\" \"/account\" -> server",
    ] {
        site.source(&format!(
            r#"
import stdlib::web::html
import stdlib::web::types
fn page() -> Html {{ text("page") }}
@placement(server)
fn server() -> Html {{ text("server") }}
app "mixed-aliases" {{ {routes} }}
"#
        ));
        let output = site.build();
        assert!(!output.status.success(), "{routes}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("conflicts"));
        assert_eq!(site.read("dist/index.html"), previous);
    }
}
