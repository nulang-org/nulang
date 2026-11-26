//! Verify document serialization through the actual static build CLI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn static_document_has_standards_mode_and_valid_void_markup() {
    let root = TempDir(std::env::temp_dir().join(format!(
        "nulang-document-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    std::fs::create_dir_all(root.0.join("src")).unwrap();
    std::fs::write(root.0.join("Nulang.toml"), "[package]\nname = \"document-fixture\"\nversion = \"0.1.0\"\n[web]\nstatic_dir = \"public\"\noutput_dir = \"dist\"\n").unwrap();
    std::fs::write(
        root.0.join("src/main.nula"),
        r#"
import stdlib::web::html
import stdlib::web::types
@placement(static)
fn home() -> Html {
    document(
        head([title("Example"), el("meta", [("charset", text("utf-8"))], [])]),
        body([el("p", [], [text("Before"), el("br", [], []), text("After")])])
    )
}
@placement(static)
fn localized() -> Html {
    document_lang("pt-BR", head([title("Exemplo")]), body([el("p", [], [text("Olá")])]))
}
app "example" {
    route "GET" "/" -> home
    route "GET" "/pt" -> localized
}
"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .current_dir(&root.0)
        .env("CARGO_BIN_EXE_nulang", env!("CARGO_BIN_EXE_nulang"))
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .args(["nula", "build", "--web"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let html = std::fs::read_to_string(root.0.join("dist/index.html")).unwrap();
    assert!(html.starts_with("<!doctype html><html"), "{html}");
    assert!(html.contains("<meta charset=\"utf-8\">"), "{html}");
    assert!(html.contains("<p>Before<br>After</p>"), "{html}");
    assert!(
        !html.contains("</meta>") && !html.contains("</br>"),
        "{html}"
    );
    assert!(
        html.starts_with("<!doctype html><html>"),
        "legacy helper must not guess a language: {html}"
    );
    let pt = std::fs::read_to_string(root.0.join("dist/pt/index.html")).unwrap();
    assert!(
        pt.starts_with("<!doctype html><html lang=\"pt-BR\">"),
        "{pt}"
    );
    assert!(pt.contains("<p>Olá</p>"), "{pt}");
}
