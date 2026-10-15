//! Opt-in Markdown content staging for the existing fail-closed Web publisher.
//! All generated files remain in the isolated build stage until publication.

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
    fn missing_or_symlinked_content_directory_cannot_generate_empty_success() {
        let f = Fixture::new();
        assert!(validate_content_directory(&f.root, "missing", "static", "dist").is_err());
    }
}
