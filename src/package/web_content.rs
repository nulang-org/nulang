//! Opt-in Markdown content staging for the existing fail-closed Web publisher.
//! All generated files remain in the isolated build stage until publication.

use crate::web::content::load_collection;
use crate::web::content_render::ContentLayout;
use crate::web::content_site::render_content_site;
use crate::web::ir::DeploymentIr;
use std::fs;
use std::path::{Component, Path, PathBuf};

fn valid_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Validate the explicit opt-in content source without following symlinks.
///
/// Input directories may not overlap compiler sources, static assets, managed
/// output, or VCS/cache directories. All paths are package-root-relative.
fn validate_content_directory(
    package_root: &Path,
    content_dir: &str,
    static_dir: &str,
    output_dir: &str,
) -> Result<PathBuf, String> {
    let relative = Path::new(content_dir);
    if !valid_relative(relative) {
        return Err(format!("invalid content_dir path: {content_dir}"));
    }
    for reserved in ["src", ".git", ".nula", static_dir, output_dir] {
        let protected = Path::new(reserved);
        if !valid_relative(protected) || overlaps(relative, protected) {
            return Err(format!(
                "content_dir {content_dir} overlaps protected project input {reserved}"
            ));
        }
    }

    let mut destination = package_root
        .canonicalize()
        .map_err(|error| format!("invalid package root: {error}"))?;
    for part in relative.components() {
        destination.push(part.as_os_str());
        let file_type = fs::symlink_metadata(&destination)
            .map_err(|error| {
                format!("cannot inspect content_dir {}: {error}", destination.display())
            })?
            .file_type();
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(format!(
                "content_dir must contain real directories, no symlinks: {}",
                destination.display()
            ));
        }
    }
    Ok(destination)
}

/// Validate every configured static source path component, not just its
/// final directory. A symlinked parent could otherwise escape asset preflight.
fn inspect_static_root(package_root: &Path, static_dir: &str) -> Result<Option<PathBuf>, String> {
    let mut path = package_root
        .canonicalize()
        .map_err(|error| format!("invalid package root: {error}"))?;
    for part in Path::new(static_dir).components() {
        path.push(part.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("static source symlink forbidden: {}", path.display()));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(format!("static source must be a directory: {}", path.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot inspect static source {}: {error}", path.display())),
        }
    }
    Ok(Some(path))
}

fn collect_static_asset_names(root: &Path) -> Result<Vec<String>, String> {
    let kind = match fs::symlink_metadata(root) {
        Ok(meta) => meta.file_type(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot inspect static root {}: {error}", root.display())),
    };
    if kind.is_symlink() || !kind.is_dir() {
        return Err(format!("static root must be a real directory: {}", root.display()));
    }

    let mut names = Vec::new();
    walk_static_assets(root, root, &mut names)?;
    names.sort();
    Ok(names)
}

fn walk_static_assets(
    root: &Path,
    dir: &Path,
    names: &mut Vec<String>,
) -> Result<(), String> {
    let items = fs::read_dir(dir)
        .map_err(|error| format!("cannot read static assets {}: {error}", dir.display()))?;
    let mut items = items
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| format!("cannot read static directory {}: {error}", dir.display()))?;
    items.sort_by_key(|item| item.file_name());

    for item in items {
        let path = item.path();
        let file_type = item
            .file_type()
            .map_err(|error| format!("cannot inspect static file {}: {error}", path.display()))?;
        if file_type.is_symlink() {
            return Err(format!("static asset symlink forbidden: {}", path.display()));
        }
        if file_type.is_dir() {
            walk_static_assets(root, &path, names)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(format!("unsupported static asset type: {}", path.display()));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("static file escaped root: {error}"))?;
        let segments = relative
            .components()
            .map(|part| {
                part.as_os_str().to_str().ok_or_else(|| {
                    format!("static file path is not UTF-8: {}", path.display())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        names.push(segments.join("/"));
    }
    Ok(())
}

/// Render and stage opt-in Markdown into an existing isolated Web build stage.
///
/// Validation and rendering happen in memory before any writes. A failed write
/// may leave partial *staged* files, but StagedWebOutput discards the entire
/// stage rather than promoting an incomplete site. Existing stage files
/// (including generated Web route artifacts) are never overwritten.
pub(super) fn stage_content(
    package_root: &Path,
    content_dir: &str,
    static_dir: &str,
    output_dir: &str,
    stage: &Path,
    ir: &DeploymentIr,
    site_title: &str,
) -> Result<usize, String> {
    let root = validate_content_directory(package_root, content_dir, static_dir, output_dir)?;
    let pages = load_collection(&root)?;
    let static_files = match inspect_static_root(package_root, static_dir)? {
        Some(root) => collect_static_asset_names(&root)?,
        None => Vec::new(),
    };
    let static_paths: Vec<&str> = static_files.iter().map(String::as_str).collect();
    let docs = render_content_site(
        &pages,
        ir,
        &static_paths,
        &ContentLayout {
            site_title,
            language: "en",
        },
    )
    .map_err(|diagnostics| diagnostics.join("\n"))?;

    // Ensure no generated page can overwrite pre-existing stage output.
    for doc in &docs {
        let destination = stage.join(&doc.artifact.artifact);
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(format!(
                "content artifact already exists in build stage: {} from {}",
                doc.artifact.artifact, doc.artifact.source_path
            ));
        }
        let mut ancestor = destination.parent();
        while let Some(dir) = ancestor {
            if !dir.starts_with(stage) {
                break;
            }
            match fs::symlink_metadata(dir) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err(format!("unsafe content output ancestor: {}", dir.display()));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("cannot inspect {}: {error}", dir.display())),
            }
            ancestor = dir.parent();
        }
    }

    let count = docs.len();
    for doc in docs {
        let destination = stage.join(&doc.artifact.artifact);
        if let Some(dir) = destination.parent() {
            fs::create_dir_all(dir).map_err(|error| {
                format!("cannot create content output {}: {error}", dir.display())
            })?;
        }
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
            .map_err(|error| {
                format!("cannot reserve content output {}: {error}", destination.display())
            })?;
        file.write_all(doc.html.as_bytes())
            .map_err(|error| format!("cannot stage content {}: {error}", destination.display()))?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "nulang_web_content_stage_{}_{}",
                std::process::id(),
                FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join("content/blog")).unwrap();
            std::fs::create_dir_all(root.join("static")).unwrap();
            std::fs::create_dir_all(root.join("staging")).unwrap();
            Self { root }
        }

        fn write(&self, path: &str, text: &str) {
            let dest = self.root.join(path);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(dest, text).unwrap();
        }

        fn build(&self, ir: &DeploymentIr) -> Result<usize, String> {
            stage_content(
                &self.root,
                "content",
                "static",
                "dist",
                &self.root.join("staging"),
                ir,
                "Demo Site",
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const PUBLIC: &str = "+++\ntitle = \"Post\"\n+++\n# Welcome\n";
    const DRAFT: &str = "+++\ntitle = \"Private\"\ndraft = true\n+++\n# Secret\n";

    #[test]
    fn stages_only_public_html_in_canonical_paths() {
        let f = Fixture::new();
        f.write("content/blog/post.md", PUBLIC);
        f.write("content/blog/draft.md", DRAFT);
        let count = f.build(&DeploymentIr::default()).unwrap();
        assert_eq!(count, 1);
        let rendered = std::fs::read_to_string(f.root.join("staging/blog/post/index.html"))
            .unwrap();
        assert!(rendered.contains("<h1>Welcome</h1>"));
        assert!(rendered.contains("Demo Site"));
        assert!(!f.root.join("staging/blog/draft/index.html").exists());
    }

    #[test]
    fn invalid_markdown_does_not_write_any_content_artifact() {
        let f = Fixture::new();
        f.write("content/blog/post.md", PUBLIC);
        f.write(
            "content/blog/bad.md",
            "+++\ntitle = \"Bad\"\n+++\n[unsupported](https://example.org)\n",
        );
        assert!(f.build(&DeploymentIr::default()).unwrap_err().contains("unsupported"));
        assert!(!f.root.join("staging/blog/post/index.html").exists());
    }

    #[test]
    fn server_get_route_blocks_staged_content_without_writes() {
        let f = Fixture::new();
        f.write("content/blog/post.md", PUBLIC);
        let mut ir = DeploymentIr::default();
        ir.routes.push(
            serde_json::from_value(serde_json::json!({
                "method": "GET",
                "path": "/blog/{slug}",
                "placement": "server",
                "artifact": null
            }))
            .unwrap(),
        );
        assert!(f.build(&ir).unwrap_err().contains("/blog/{slug}"));
        assert!(!f.root.join("staging/blog/post/index.html").exists());
    }

    #[test]
    fn user_owned_static_html_blocks_staged_content() {
        let f = Fixture::new();
        f.write("content/blog/post.md", PUBLIC);
        f.write("static/blog/post/index.html", "owned asset");
        assert!(f.build(&DeploymentIr::default()).unwrap_err().contains("static asset"));
        assert!(!f.root.join("staging/blog/post/index.html").exists());
    }

    #[test]
    fn existing_staged_file_never_gets_overwritten() {
        let f = Fixture::new();
        f.write("content/blog/post.md", PUBLIC);
        f.write("staging/blog/post/index.html", "existing rendered route");
        let error = f.build(&DeploymentIr::default()).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read_to_string(f.root.join("staging/blog/post/index.html")).unwrap(),
            "existing rendered route"
        );
    }

    #[test]
    fn rejects_invalid_or_overlapping_content_source_paths() {
        let f = Fixture::new();
        for path in [
            "../private",
            "/tmp/outside",
            ".",
            "src",
            "src/notes",
            ".git",
            ".nula",
            "dist",
            "static",
            "static/subdir",
        ] {
            assert!(
                validate_content_directory(&f.root, path, "static", "dist").is_err(),
                "{path} must not be a content root"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn static_symlinks_are_not_followed_during_content_preflight() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        f.write("content/blog/post.md", PUBLIC);
        f.write("outside.txt", "sensitive content");
        symlink(f.root.join("outside.txt"), f.root.join("static/leak.txt")).unwrap();
        let error = f.build(&DeploymentIr::default()).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(!f.root.join("staging/blog/post/index.html").exists());
    }

    #[test]
    fn failed_content_build_preserves_last_published_site() {
        let f = Fixture::new();
        f.write("dist/nulang-app.ir.json", "{}");
        f.write("dist/index.html", "last good site");
        f.write(
            "content/blog/bad.md",
            "+++\ntitle = \"Bad\"\n+++\n[unsupported](https://example.org)\n",
        );

        let staged = crate::package::web_output::StagedWebOutput::new(
            &f.root,
            "dist",
            "static",
        )
        .unwrap();
        let result = stage_content(
            &f.root,
            "content",
            "static",
            "dist",
            staged.path(),
            &DeploymentIr::default(),
            "Demo",
        );
        assert!(result.is_err());
        drop(staged);

        assert_eq!(
            std::fs::read_to_string(f.root.join("dist/index.html")).unwrap(),
            "last good site"
        );
    }

    #[test]
    fn successful_content_stage_publishes_only_after_final_promotion() {
        let f = Fixture::new();
        f.write("dist/nulang-app.ir.json", "{}");
        f.write("dist/index.html", "last good site");
        f.write("content/blog/post.md", PUBLIC);

        let staged = crate::package::web_output::StagedWebOutput::new(
            &f.root,
            "dist",
            "static",
        )
        .unwrap();
        let count = stage_content(
            &f.root,
            "content",
            "static",
            "dist",
            staged.path(),
            &DeploymentIr::default(),
            "Demo",
        )
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            std::fs::read_to_string(f.root.join("dist/index.html")).unwrap(),
            "last good site"
        );

        std::fs::write(staged.path().join("nulang-app.ir.json"), "{}").unwrap();
        staged.publish().unwrap();
        assert!(f.root.join("dist/blog/post/index.html").is_file());
        assert!(!f.root.join("dist/index.html").exists());
    }

    #[cfg(unix)]
    #[test]
    fn static_parent_symlink_is_not_followed_in_content_build() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        f.write("content/blog/post.md", PUBLIC);
        std::fs::create_dir_all(f.root.join("real-assets/public")).unwrap();
        symlink(
            f.root.join("real-assets"),
            f.root.join("asset-link"),
        )
        .unwrap();
        let err = stage_content(
            &f.root,
            "content",
            "asset-link/public",
            "dist",
            &f.root.join("staging"),
            &DeploymentIr::default(),
            "Demo",
        )
        .unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(!f.root.join("staging/blog/post/index.html").exists());
    }

    #[test]
    fn missing_or_symlinked_content_directory_cannot_generate_empty_success() {
        let f = Fixture::new();
        assert!(validate_content_directory(&f.root, "missing", "static", "dist").is_err());
    }
}
