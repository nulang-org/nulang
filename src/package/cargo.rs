//! Locked Cargo cdylib builds for explicitly opted-in Nulang packages.
//! Cargo.lock remains the authoritative Rust dependency lockfile.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::package::manifest::RustSection;
use crate::types::{NuError, NuResult, Span};

fn package_error(message: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: message.into(),
        span: Span::default(),
    }
}

fn native_manifest(root: &Path, config: &RustSection) -> NuResult<PathBuf> {
    let relative = Path::new(&config.manifest);
    if relative.as_os_str().is_empty()
        || relative.file_name() != Some(std::ffi::OsStr::new("Cargo.toml"))
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(package_error(
            "rust.manifest must be a relative path to Cargo.toml inside the package",
        ));
    }

    let canonical_root = root
        .canonicalize()
        .map_err(|e| package_error(format!("cannot locate Nulang package root: {e}")))?;
    let canonical = canonical_root.join(relative).canonicalize().map_err(|e| {
        package_error(format!(
            "cannot find rust.manifest {}: {e}",
            relative.display()
        ))
    })?;
    if !canonical.starts_with(&canonical_root) {
        return Err(package_error(
            "rust.manifest must be inside the Nulang package (symlink escape rejected)",
        ));
    }
    Ok(canonical)
}

fn cargo_output(mut command: Command, action: &str) -> NuResult<std::process::Output> {
    let output = command
        .output()
        .map_err(|e| package_error(format!("could not run Cargo {action}: {e}")))?;
    if !output.status.success() {
        return Err(package_error(format!(
            "Cargo {action} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}

/// Build one locked cdylib, using Cargo artifact messages to locate its output.
pub(crate) fn build_native(root: &Path, config: &RustSection) -> NuResult<()> {
    let manifest = native_manifest(root, config)?;

    let mut metadata = Command::new("cargo");
    metadata
        .args([
            "metadata",
            "--no-deps",
            "--locked",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(&manifest);
    let meta = cargo_output(metadata, "metadata")?;
    let document: serde_json::Value = serde_json::from_slice(&meta.stdout)
        .map_err(|e| package_error(format!("cannot decode Cargo metadata: {e}")))?;
    let package = document["packages"]
        .as_array()
        .and_then(|packages| {
            packages
                .iter()
                .find(|p| p["manifest_path"].as_str().map(Path::new) == Some(manifest.as_path()))
        })
        .ok_or_else(|| {
            package_error("rust.manifest must identify a Cargo package, not a virtual workspace")
        })?;
    let package_name = package["name"]
        .as_str()
        .ok_or_else(|| package_error("Cargo package has no name"))?;
    let package_id = package["id"]
        .as_str()
        .ok_or_else(|| package_error("Cargo package has no id"))?;
    let cdylibs: Vec<&serde_json::Value> = package["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"))
        })
        .collect();
    if cdylibs.len() != 1 {
        return Err(package_error(format!(
            "Cargo package {package_name} must declare exactly one cdylib target"
        )));
    }
    let target_name = cdylibs[0]["name"]
        .as_str()
        .ok_or_else(|| package_error("Cargo cdylib has no name"))?;

    eprintln!("  Building locked Cargo adapter {package_name}...");
    let mut build = Command::new("cargo");
    build
        .args(["build", "--locked", "--release", "--manifest-path"])
        .arg(&manifest)
        .args(["--package", package_name, "--message-format=json"]);
    let result = cargo_output(build, "build")?;
    let expected_filename = format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        target_name,
        std::env::consts::DLL_SUFFIX
    );
    let artifact = result
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find(|message| {
            message["reason"] == "compiler-artifact"
                && message["package_id"] == package_id
                && message["target"]["name"] == target_name
                && message["target"]["kind"]
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"))
        })
        .and_then(|message| message["filenames"].as_array().cloned())
        .and_then(|files| {
            files.into_iter().find_map(|file| {
                let path = file.as_str().map(PathBuf::from)?;
                (path.file_name()? == std::ffi::OsStr::new(&expected_filename)).then_some(path)
            })
        })
        .ok_or_else(|| {
            package_error(format!(
                "Cargo built {package_name} without reporting the expected cdylib artifact"
            ))
        })?;

    let out_dir = root.join(".nula/native");
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| package_error(format!("cannot create {}: {e}", out_dir.display())))?;
    let destination = out_dir.join(expected_filename);
    let staged = destination.with_extension("nula-tmp");
    std::fs::copy(&artifact, &staged).map_err(|e| {
        package_error(format!(
            "cannot copy Cargo artifact {}: {e}",
            artifact.display()
        ))
    })?;
    std::fs::rename(&staged, &destination).map_err(|e| {
        package_error(format!(
            "cannot install Cargo adapter {}: {e}",
            destination.display()
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn cargo_bridge_rejects_parent_escape() {
        let root = std::env::temp_dir();
        let err = build_native(
            &root,
            &RustSection {
                manifest: "../Cargo.toml".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{err}").contains("relative"), "{err}");
    }

    #[test]
    fn cargo_bridge_rejects_symlink_escape() {
        let root = std::env::temp_dir().join(format!(
            "nulang-cargo-bridge-link-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        {
            let outside = std::env::temp_dir().join(format!(
                "nulang-cargo-bridge-outside-{}",
                std::process::id()
            ));
            fs::create_dir_all(&outside).unwrap();
            fs::write(
                outside.join("Cargo.toml"),
                r#"[package]
name = "outside"
version = "0.1.0"
"#,
            )
            .unwrap();
            std::os::unix::fs::symlink(&outside, root.join("native")).unwrap();
            let err = build_native(
                &root,
                &RustSection {
                    manifest: "native/Cargo.toml".into(),
                },
            )
            .unwrap_err();
            assert!(
                format!("{err}").contains("symlink escape rejected"),
                "{err}"
            );
            fs::remove_dir_all(&outside).unwrap();
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cargo_bridge_builds_a_locked_cdylib() {
        let root =
            std::env::temp_dir().join(format!("nulang-cargo-bridge-test-{}", std::process::id()));
        let native = root.join("native");
        fs::create_dir_all(native.join("src")).unwrap();
        fs::write(
            native.join("Cargo.toml"),
            r#"[package]
name = "nulang_bridge_test"
version = "0.1.0"
edition = "2021"
[lib]
crate-type = ["cdylib"]
"#,
        )
        .unwrap();
        fs::write(
            native.join("src/lib.rs"),
            r#"#[no_mangle] pub extern "C" fn add_one(x: i64) -> i64 { x + 1 }"#,
        )
        .unwrap();
        let unlocked = build_native(
            &root,
            &RustSection {
                manifest: "native/Cargo.toml".into(),
            },
        )
        .expect_err("Cargo bridge must not create a missing lockfile");
        assert!(format!("{unlocked}").contains("Cargo "), "{unlocked}");

        let status = Command::new("cargo")
            .args(["generate-lockfile", "--offline", "--manifest-path"])
            .arg(native.join("Cargo.toml"))
            .status()
            .unwrap();
        assert!(status.success());
        build_native(
            &root,
            &RustSection {
                manifest: "native/Cargo.toml".into(),
            },
        )
        .unwrap();
        let output = root.join(".nula/native").join(format!(
            "{}nulang_bridge_test{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ));
        assert!(
            output.exists(),
            "missing built adapter: {}",
            output.display()
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
