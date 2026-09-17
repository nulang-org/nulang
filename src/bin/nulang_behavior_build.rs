//! Experimental single-pass Wasm + Behavior Manifest builder (RFC 0020).
//!
//! This is intentionally separate from the stable `nulang` CLI while the
//! v0alpha1 contract is validated. Unlike `nulang_behavior_manifest`, it is
//! the compiler process that produces the Wasm artifact, so `current_exe()` is
//! the exact compiler artifact represented in manifest provenance.

#[cfg(feature = "wasm-backend")]
mod enabled {
    use std::path::{Path, PathBuf};

    use nulang::behavior_build::{compile_wasm_behavior, BehaviorBuildInput};

    #[derive(Debug)]
    struct Options {
        source: PathBuf,
        package_name: String,
        package_version: String,
        dependency_lock: PathBuf,
        out: PathBuf,
        manifest_out: PathBuf,
        grants: Vec<String>,
        aot: bool,
        deny_warnings: bool,
    }

    pub fn main() {
        if let Err(error) = run() {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }

    fn run() -> Result<(), String> {
        let options = parse_args(std::env::args().skip(1).collect())?;
        let dependency_bytes = std::fs::read(&options.dependency_lock).map_err(|error| {
            format!(
                "cannot read dependency lock '{}': {error}",
                options.dependency_lock.display()
            )
        })?;

        let compiler_path = std::env::current_exe()
            .map_err(|error| format!("cannot locate compiler executable: {error}"))?;
        let compiler_bytes = std::fs::read(&compiler_path).map_err(|error| {
            format!(
                "cannot read compiler executable '{}': {error}",
                compiler_path.display()
            )
        })?;

        let output = compile_wasm_behavior(BehaviorBuildInput {
            source_path: &options.source,
            package_name: &options.package_name,
            package_version: &options.package_version,
            dependency_bytes: &dependency_bytes,
            compiler_implementation: "nulang-rust",
            compiler_version: env!("CARGO_PKG_VERSION"),
            compiler_bytes: &compiler_bytes,
            with_capabilities: &options.grants,
            deny_warnings: options.deny_warnings,
        })
        .map_err(|error| format!("integrated build failed: {error}"))?;

        ensure_parent(&options.out)?;
        std::fs::write(&options.out, &output.wasm_bytes).map_err(|error| {
            format!("cannot write Wasm '{}': {error}", options.out.display())
        })?;

        let manifest_json = output
            .manifest
            .to_canonical_json()
            .map_err(|error| format!("cannot serialize behavior manifest: {error}"))?;
        ensure_parent(&options.manifest_out)?;
        std::fs::write(&options.manifest_out, manifest_json).map_err(|error| {
            format!(
                "cannot write behavior manifest '{}': {error}",
                options.manifest_out.display()
            )
        })?;

        println!(
            "Wrote {} ({} bytes)",
            options.out.display(),
            output.wasm_bytes.len()
        );
        println!("Wrote {}", options.manifest_out.display());

        if options.aot {
            let cwasm = cwasm_path(&options.out);
            let wasm = options.out.to_string_lossy().into_owned();
            let cwasm_string = cwasm.to_string_lossy().into_owned();
            nulang::wasm_runtime::aot_compile(&wasm, &cwasm_string)
                .map_err(|error| format!("AOT compilation failed: {error}"))?;
            println!("Wrote {} (precompiled)", cwasm.display());
        }

        Ok(())
    }

    fn parse_args(args: Vec<String>) -> Result<Options, String> {
        if args.iter().any(|arg| arg == "-h" || arg == "--help") {
            print_help();
            std::process::exit(0);
        }

        let mut source = None;
        let mut package_name = None;
        let mut package_version = None;
        let mut dependency_lock = None;
        let mut out = None;
        let mut manifest_out = None;
        let mut grants = Vec::new();
        let mut aot = false;
        let mut deny_warnings = false;

        let mut index = 0usize;
        while index < args.len() {
            let flag = &args[index];
            let value = |index: &mut usize| -> Result<String, String> {
                *index += 1;
                args.get(*index)
                    .cloned()
                    .ok_or_else(|| format!("{flag} requires a value"))
            };

            match flag.as_str() {
                "--source" => source = Some(PathBuf::from(value(&mut index)?)),
                "--package-name" => package_name = Some(value(&mut index)?),
                "--package-version" => package_version = Some(value(&mut index)?),
                "--dependency-lock" => {
                    dependency_lock = Some(PathBuf::from(value(&mut index)?));
                }
                "--out" => out = Some(PathBuf::from(value(&mut index)?)),
                "--manifest-out" => {
                    manifest_out = Some(PathBuf::from(value(&mut index)?));
                }
                "--with" => {
                    let raw = value(&mut index)?;
                    grants.extend(
                        raw.split(',')
                            .map(str::trim)
                            .filter(|part| !part.is_empty())
                            .map(ToOwned::to_owned),
                    );
                }
                "--aot" => aot = true,
                "--deny-warnings" => deny_warnings = true,
                other => return Err(format!("unknown option '{other}' (run with --help)")),
            }
            index += 1;
        }

        let source = source.ok_or_else(|| "missing required --source".to_string())?;
        let package_name =
            package_name.ok_or_else(|| "missing required --package-name".to_string())?;
        let package_version =
            package_version.ok_or_else(|| "missing required --package-version".to_string())?;
        let dependency_lock = dependency_lock
            .ok_or_else(|| "missing required --dependency-lock".to_string())?;
        let out = out.ok_or_else(|| "missing required --out".to_string())?;
        let manifest_out = manifest_out.unwrap_or_else(|| default_manifest_path(&out));

        Ok(Options {
            source,
            package_name,
            package_version,
            dependency_lock,
            out,
            manifest_out,
            grants,
            aot,
            deny_warnings,
        })
    }

    fn ensure_parent(path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create '{}': {error}", parent.display()))?;
            }
        }
        Ok(())
    }

    fn default_manifest_path(wasm: &Path) -> PathBuf {
        wasm.with_extension("behavior.json")
    }

    fn cwasm_path(wasm: &Path) -> PathBuf {
        if wasm.extension().and_then(|extension| extension.to_str()) == Some("wasm") {
            wasm.with_extension("cwasm")
        } else {
            let mut name = wasm.as_os_str().to_os_string();
            name.push(".cwasm");
            PathBuf::from(name)
        }
    }

    fn print_help() {
        println!(
            "nulang_behavior_build (experimental RFC 0020 v0alpha1)\n\
             \n\
             Usage:\n\
               nulang_behavior_build \\\n                 --source <src/main.nula> \\\n                 --package-name <name> \\\n                 --package-version <version> \\\n                 --dependency-lock <Nulang.lock> \\\n                 --out <package.wasm> \\\n                 [--manifest-out <package.behavior.json>] \\\n                 [--with <fs,net,os>] \\\n                 [--aot] [--deny-warnings]\n\
             \n\
             This compiler resolves/import-checks once and emits both the Wasm\n\
             artifact and Behavior Manifest from the same checked compilation\n\
             unit. Missing dependency provenance or authority grants fail closed."
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn manifest_path_replaces_wasm_extension() {
            assert_eq!(
                default_manifest_path(Path::new(".nula/dist/demo.wasm")),
                PathBuf::from(".nula/dist/demo.behavior.json")
            );
        }

        #[test]
        fn required_dependency_provenance_fails_closed() {
            let error = parse_args(vec![
                "--source".into(),
                "src/main.nula".into(),
                "--package-name".into(),
                "demo".into(),
                "--package-version".into(),
                "0.1.0".into(),
                "--out".into(),
                "demo.wasm".into(),
            ])
            .unwrap_err();
            assert!(error.contains("--dependency-lock"));
        }
    }
}

#[cfg(feature = "wasm-backend")]
fn main() {
    enabled::main();
}

#[cfg(not(feature = "wasm-backend"))]
fn main() {
    eprintln!("error: nulang_behavior_build requires the 'wasm-backend' feature");
    std::process::exit(2);
}
