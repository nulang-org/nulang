use std::env;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;

fn main() {
    // Only needed when linking against PyO3; skip entirely for builds with
    // the "python" feature disabled (`cargo:rerun-if-changed` for the
    // feature flag itself so re-enabling it re-triggers this build script).
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PYTHON");
    if env::var_os("CARGO_FEATURE_PYTHON").is_none() {
        return;
    }

    // Cross-compilation: the host libpython is the wrong architecture and
    // must not be linked into the target binary. pyo3's cross config
    // (PYO3_CROSS_LIB_DIR / sysconfigdata) handles target linking instead.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if !target_arch.is_empty() && target_arch != std::env::consts::ARCH {
        return;
    }

    // On some Linux distributions (e.g. Fedora) the libpython3.X.so symlink
    // installed by the -devel package is missing while libpython3.X.so.1.0 is
    // present. PyO3 emits -lpython3.X and the linker fails because it cannot
    // find the unversioned .so name. We work around this by creating a
    // libpython3.X.so symlink in OUT_DIR and adding OUT_DIR to the library
    // search path.
    //
    // Unix-only: `std::os::unix::fs::symlink` does not exist on Windows,
    // and the searched paths are Linux multiarch dirs anyway. On Windows
    // pyo3 links the Python import lib directly.
    #[cfg(unix)]
    {
        let out_dir = env::var_os("OUT_DIR")
            .map(PathBuf::from)
            .expect("OUT_DIR not set");

        let version = detect_python_version();
        let soname = format!("libpython{}.so", version);

        if let Some(lib) = find_python_lib(&version) {
            let link = out_dir.join(&soname);
            if link.exists() || std::os::unix::fs::symlink(&lib, &link).is_ok() {
                println!("cargo:rustc-link-search=native={}", out_dir.display());
                println!("cargo:rustc-link-lib=python{}", version);
            }
            println!("cargo:rerun-if-changed=build.rs");
        }
    }
}

#[cfg(unix)]
fn detect_python_version() -> String {
    env::var("PYO3_PYTHON")
        .ok()
        .and_then(|exe| python_version_from_exe(&exe))
        .or_else(|| python_version_from_exe("python3"))
        .unwrap_or_else(|| "3.14".to_string())
}

#[cfg(unix)]
fn python_version_from_exe(exe: &str) -> Option<String> {
    let output = Command::new(exe).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        text
    };
    // "Python 3.14.6" -> "3.14"
    let parts: Vec<_> = text.split_whitespace().collect();
    parts.get(1).map(|v| {
        let mut iter = v.split('.');
        let major = iter.next().unwrap_or("3");
        let minor = iter.next().unwrap_or("0");
        format!("{}.{}", major, minor)
    })
}

#[cfg(unix)]
fn find_python_lib(version: &str) -> Option<PathBuf> {
    let search_dirs = ["/usr/lib64", "/lib64", "/usr/lib/x86_64-linux-gnu"];
    let candidates = [
        format!("libpython{}.so.1.0", version),
        format!("libpython{}.so", version),
    ];
    for dir in &search_dirs {
        for name in &candidates {
            let path = Path::new(dir).join(name);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}
