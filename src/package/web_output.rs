#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "nulang_web_output_{}_{}_{}",
            label,
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn successful_publish_replaces_stale_output_instead_of_merging() {
        let root = fixture("replace");
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/obsolete.html"), "old").unwrap();
        let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
        std::fs::write(pending.path().join("index.html"), "new").unwrap();

        pending.publish().unwrap();

        assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "new");
        assert!(!root.join("dist/obsolete.html").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dropped_or_failed_build_preserves_previous_output() {
        let root = fixture("discard");
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/index.html"), "old").unwrap();
        {
            let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
            std::fs::write(pending.path().join("index.html"), "new").unwrap();
            assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "old");
        }
        assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "old");
        let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
        std::fs::write(pending.path().join("index.html"), "later").unwrap();
        pending.publish().unwrap();
        assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "later");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_install_renames_previous_output_back() {
        let root = fixture("rollback");
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/index.html"), "old").unwrap();
        let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
        std::fs::write(pending.path().join("index.html"), "new").unwrap();

        let result = pending.publish_with(|_, _| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected install failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "old");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_builds_are_rejected_without_disturbing_existing_output() {
        let root = fixture("lock");
        let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
        assert!(StagedWebOutput::new(&root, "dist", "public").is_err());
        drop(pending);
        assert!(StagedWebOutput::new(&root, "dist", "public").is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reject_output_paths_that_can_replace_project_inputs() {
        let root = fixture("unsafe");
        for output in ["", ".", "..", "../escape", "/tmp/out", "src", "src/generated", ".git", ".nula", "public", "public/build"] {
            assert!(
                StagedWebOutput::new(&root, output, "public").is_err(),
                "output {output:?} must not be writable"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn reject_symlinked_output_or_parent() {
        use std::os::unix::fs::symlink;
        let root = fixture("symlink");
        std::fs::create_dir(root.join("outside")).unwrap();
        symlink(root.join("outside"), root.join("dist")).unwrap();
        assert!(StagedWebOutput::new(&root, "dist", "public").is_err());
        symlink(root.join("outside"), root.join("alias")).unwrap();
        assert!(StagedWebOutput::new(&root, "alias/dist", "public").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
