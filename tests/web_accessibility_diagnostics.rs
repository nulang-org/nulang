//! Compiler diagnostics deliberately lint syntax only, not dynamic markup.

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::types::NuWarning;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn warnings(source: &str) -> Vec<NuWarning> {
    let mut parser = Parser::new(Lexer::new(source).lex().unwrap());
    parser.parse_module().expect("parse HTML fixture");
    parser.take_warnings()
}

#[test]
fn static_html_syntax_reports_missing_language_alt_and_names() {
    let source = r#"fn page() {
    <html><img src="photo.png" /><button /></html>
}"#;
    let found = warnings(source);
    for (code, tag) in [("W0201", "<html>"), ("W0202", "<img"), ("W0203", "<button")] {
        let warning = found
            .iter()
            .find(|warning| warning.code == code)
            .expect(code);
        assert_eq!(warning.span.start as usize, source.find(tag).unwrap());
    }
    assert_eq!(found.len(), 3);
}

#[test]
fn literal_web_effect_is_linted_without_guessing_dynamic_markup() {
    let source = r#"fn page() {
    let image = perform Web.html("IMG", [], [])
    let document = perform Web.html("html", [("lang", perform Web.text(""))], [])
    let button = perform Web.html("button", [], [])
    perform Web.html("input", [("type", perform Web.text("image"))], [])
}"#;
    let found = warnings(source);
    assert_eq!(found.iter().filter(|w| w.code == "W0202").count(), 2);
    assert!(found.iter().any(|w| w.code == "W0201"));
    assert!(found.iter().any(|w| w.code == "W0203"));
}

#[test]
fn compliant_dynamic_and_unrelated_expressions_do_not_warn() {
    let source = r#"fn page(attrs, alt, language) {
    let decorative = <img alt="" />
    let translated = <html lang={language}><button>Save</button></html>
    let meaningful = perform Web.html("img", [("alt", alt)], [])
    let explicit = perform Web.html("button", [("aria-label", perform Web.text("Save"))], [])
    let external = perform Web.html("button", [("id", perform Web.text("external-label"))], [])
    let dynamic = perform Web.html("img", attrs, [])
    let key = perform Web.html("img", [(language, alt)], [])
    let labelled = perform Web.html("input", [("type", perform Web.text("text"))], [])
    el("img", [], [])
}"#;
    assert!(warnings(source).is_empty());
}

#[test]
fn static_void_children_are_reported_before_runtime() {
    let found = warnings(r#"fn page() { <br>unexpected</br> }"#);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].code, "W0204");
}

#[test]
fn empty_language_and_image_button_alt_are_reported_but_named_links_are_not() {
    let source = r#"fn page() {
    let document = <html lang=""><body /></html>
    let image_button = perform Web.html("input", [("type", perform Web.text("IMAGE")), ("alt", perform Web.text(""))], [])
    let named = perform Web.html("a", [("href", perform Web.text("/go"))], [perform Web.text("Go")])
    perform Web.html("a", [("href", perform Web.text("/go"))], [])
}"#;
    let found = warnings(source);
    assert_eq!(found.len(), 3);
    assert!(found.iter().any(|warning| warning.code == "W0201"));
    assert!(found.iter().any(|warning| warning.code == "W0202"));
    assert!(found.iter().any(|warning| warning.code == "W0203"));
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn cli_reports_warning_and_deny_warnings_escalates_it() {
    let file = TempFile(std::env::temp_dir().join(format!(
        "nulang-a11y-{}-{}.nula", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    std::fs::write(
        &file.0,
        r#"@placement(static)
fn main() { perform Web.html("img", [], []) }
"#,
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--check")
        .arg(&file.0)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("W0202"));
    let strict = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["--deny-warnings", "--check"])
        .arg(&file.0)
        .output()
        .unwrap();
    assert!(!strict.status.success());
    let stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(
        stderr.contains("W0202") && stderr.contains("--deny-warnings"),
        "{stderr}"
    );
}
