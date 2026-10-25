//! Convention-based, additive resource route scaffolding for the Web template.
//! Generated routes are GET-only; storage, auth and mutation policies are intentionally not fabricated.

use crate::types::{NuError, NuResult, Span};
use std::fs;
use std::io::Write;
use std::path::Path;

const HANDLERS_START: &str = "// nula:resources:handlers-start";
const HANDLERS_END: &str = "// nula:resources:handlers-end";
const ROUTES_START: &str = "// nula:resources:routes-start";
const ROUTES_END: &str = "    // nula:resources:routes-end";

fn invalid(reason: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: reason.into(),
        span: Span::default(),
    }
}

fn resource_slug(name: &str) -> NuResult<String> {
    if name.len() > 64
        || !name.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        || !name.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(invalid("resource name must be an ASCII PascalCase identifier, starting with A-Z (max 64 characters)"));
    }
    let mut slug = String::new();
    let mut previous_lower_or_digit = false;
    for ch in name.chars() {
        if ch.is_ascii_uppercase() && previous_lower_or_digit {
            slug.push('_');
        }
        slug.push(ch.to_ascii_lowercase());
        previous_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    // Deliberately simple, predictable convention. Irregular nouns can be
    // handled with manually authored routes until explicit slug overrides land.
    if slug.ends_with('y')
        && !matches!(slug.chars().rev().nth(1), Some('a' | 'e' | 'i' | 'o' | 'u'))
    {
        slug.pop();
        slug.push_str("ies");
    } else if !slug.ends_with('s') {
        slug.push('s');
    }
    Ok(slug)
}

fn expect_unique_marker(source: &str, marker: &str) -> NuResult<()> {
    if source.matches(marker).count() != 1 {
        return Err(invalid(format!(
            "resource scaffolding requires exactly one '{}' marker in src/main.nula; use a fresh 'web' template or add explicit extension points",
            marker
        )));
    }
    Ok(())
}

/// Append a read-only resource index and creation placeholder to a web-template app.
/// Generated handlers are ordinary Nulang code and routes pass through Web Contract IR.
pub fn generate_resource(root: &Path, name: &str) -> NuResult<()> {
    let plural = resource_slug(name)?;
    let manifest = root.join("Nulang.toml");
    let manifest_src = fs::read_to_string(&manifest)
        .map_err(|e| invalid(format!("cannot read {}: {}", manifest.display(), e)))?;
    if !manifest_src.lines().any(|line| line.trim() == "[web]") {
        return Err(invalid(
            "resource scaffolding requires a web package ([web] in Nulang.toml)",
        ));
    }
    let main = root.join("src/main.nula");
    let source = fs::read_to_string(&main)
        .map_err(|e| invalid(format!("cannot read {}: {}", main.display(), e)))?;
    for marker in [HANDLERS_START, HANDLERS_END, ROUTES_START, ROUTES_END] {
        expect_unique_marker(&source, marker)?;
    }
    if source.find(HANDLERS_START) > source.find(HANDLERS_END)
        || source.find(ROUTES_START) > source.find(ROUTES_END)
    {
        return Err(invalid("resource extension point markers are out of order"));
    }
    if source.contains(&format!("fn {plural}_index("))
        || source.contains(&format!("\"/{plural}\""))
        || source.contains(&format!("\"/{plural}/new\""))
    {
        return Err(invalid(format!(
            "resource route '{plural}' already exists; source left unchanged"
        )));
    }

    let handlers = format!(
        r#"
fn {plural}_index() -> Html {{
    document(
        head([title("{name}")]),
        body([
            el("h1", [], [text("{name}")]),
            el("p", [], [text("Read-only resource scaffold. Add persistence and authorization before exposing mutations.")])
        ])
    )
}}

fn {plural}_new() -> Html {{
    document(
        head([title("New {name}")]),
        body([
            el("h1", [], [text("New {name}")]),
            el("p", [], [text("Add a validated form and an authorized transaction before enabling creation.")])
        ])
    )
}}
"#
    );
    let routes = format!(
        "    route \"GET\" \"/{plural}\" -> {plural}_index\n    route \"GET\" \"/{plural}/new\" -> {plural}_new\n"
    );
    let next = source
        .replacen(HANDLERS_END, &format!("{handlers}\n{HANDLERS_END}"), 1)
        .replacen(ROUTES_END, &format!("{routes}{ROUTES_END}"), 1);

    // A temporary sibling avoids truncating main.nula if a write is interrupted.
    let temporary = main.with_file_name(format!(
        "main.nula.nula-generate-{}.tmp",
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| invalid(format!("cannot create temporary resource file: {e}")))?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(next.as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&temporary, fs::metadata(&main)?.permissions())?;
        fs::rename(&temporary, &main)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|e| invalid(format!("failed to write {}: {}", main.display(), e)))?;
    println!(
        "Generated GET /{plural} and GET /{plural}/new in {}",
        main.display()
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod symlink_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Cleanup, std::path::PathBuf, std::path::PathBuf, String) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "nulang_resource_symlink_guard_{}_{}",
            std::process::id(),
            nonce
        ));
        let root = base.join("package");
        let outside = base.join("external");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("Nulang.toml"), "[web]\n").unwrap();
        let original = "// nula:resources:handlers-start\n// nula:resources:handlers-end\napp \"x\" {\n// nula:resources:routes-start\n    // nula:resources:routes-end\n}\n".to_string();
        fs::write(outside.join("main.nula"), &original).unwrap();
        (Cleanup(base), root, outside, original)
    }

    #[test]
    fn symlinked_src_is_rejected_before_touching_external_main() {
        let (_cleanup, root, outside, original) = fixture();
        symlink(&outside, root.join("src")).unwrap();
        let result = generate_resource(&root, "Billboard");
        assert!(
            result.is_err(),
            "must not follow package src/ outside the project"
        );
        assert_eq!(
            fs::read_to_string(outside.join("main.nula")).unwrap(),
            original
        );
        assert!(!outside
            .join(format!(
                "main.nula.nula-generate-{}.tmp",
                std::process::id()
            ))
            .exists());
    }

    #[test]
    fn symlinked_main_is_rejected_without_replacing_user_link() {
        let (_cleanup, root, outside, original) = fixture();
        fs::create_dir_all(root.join("src")).unwrap();
        symlink(outside.join("main.nula"), root.join("src/main.nula")).unwrap();
        let result = generate_resource(&root, "Billboard");
        assert!(result.is_err(), "must not replace a symlinked main.nula");
        assert!(fs::symlink_metadata(root.join("src/main.nula"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(outside.join("main.nula")).unwrap(),
            original
        );
    }
}
