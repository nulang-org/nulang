//! Staged web-build output publication. Tests are written first to specify
//! rollback and retention behavior before introducing the implementation.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "nulang-web-stage-{name}-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn output(&self) -> PathBuf {
            self.0.join("dist")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn failed_staged_build_does_not_mutate_previous_output() {
        let fixture = Fixture::new("failed");
        fs::create_dir_all(&fixture.output()).unwrap();
        fs::write(fixture.output().join("index.html"), b"old").unwrap();
        {
            let stage = StagedWebOutput::prepare(&fixture.output()).unwrap();
            fs::write(stage.path().join("index.html"), b"partial").unwrap();
            // Simulate build failure by dropping the stage without publication.
        }
        assert_eq!(fs::read(fixture.output().join("index.html")).unwrap(), b"old");
    }

    #[test]
    fn successful_staged_build_promotes_new_output_and_keeps_other_assets() {
        let fixture = Fixture::new("success");
        fs::create_dir_all(&fixture.output()).unwrap();
        fs::write(fixture.output().join("index.html"), b"old").unwrap();
        fs::write(fixture.output().join("unmanaged.txt"), b"retain").unwrap();
        let stage = StagedWebOutput::prepare(&fixture.output()).unwrap();
        fs::write(stage.path().join("index.html"), b"new").unwrap();
        stage.publish().unwrap();
        assert_eq!(fs::read(fixture.output().join("index.html")).unwrap(), b"new");
        assert_eq!(
            fs::read(fixture.output().join("unmanaged.txt")).unwrap(),
            b"retain"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staged_build_refuses_symlinked_prior_output() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("symlink");
        let private = fixture.0.join("private");
        fs::create_dir_all(&private).unwrap();
        fs::write(private.join("index.html"), b"secret").unwrap();
        symlink(&private, fixture.output()).unwrap();
        assert!(StagedWebOutput::prepare(&fixture.output()).is_err());
        assert_eq!(fs::read(private.join("index.html")).unwrap(), b"secret");
    }
}
