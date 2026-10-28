//! Resolve source standard-library modules consistently across compilation,
//! Web Contract IR extraction and application execution.
//!
//! Explicit NULANG_STDLIB wins. Installed binaries use adjacent stdlib files;
//! source-built CLIs can use their compilation checkout even when invoked from
//! a newly scaffolded application in another directory.

use std::path::{Path, PathBuf};

pub(crate) fn resolve_module(module: &str) -> PathBuf {
    let relative = format!(
        "{}.nula",
        module.replace("::", std::path::MAIN_SEPARATOR_STR)
    );

    // Preserve explicitly configured paths, including nonexistent ones:
    // spelling mistakes should fail, not silently select a different library.
    if let Some(explicit) = std::env::var_os("NULANG_STDLIB") {
        return PathBuf::from(explicit).join(&relative);
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("stdlib").join(&relative);
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("src").join("stdlib").join(&relative);
        if candidate.is_file() {
            return candidate;
        }
    }

    // The root is baked in for the source-built toolchain. In a packaged
    // release it may not exist, in which case the fallback remains explicit.
    let compiled_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("stdlib")
        .join(&relative);
    if compiled_root.is_file() {
        return compiled_root;
    }

    PathBuf::from("src").join("stdlib").join(relative)
}
