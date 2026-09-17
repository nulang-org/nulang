//! Experimental RFC 0020 Behavior Manifest emitter.
//!
//! This binary is intentionally separate from the stable `nulang` / `nula`
//! CLI while v0alpha1 is being validated. It refuses source-level imports
//! rather than pretending a single-file analysis represents a multi-module
//! package. The eventual package integration must reuse the import-aware
//! compiler frontend and emit from the exact checked compilation unit.

use std::path::{Path, PathBuf};

use nulang::ast::Decl;
use nulang::behavior_manifest::{ArtifactKind, BehaviorManifest, ManifestBuildInput};
use nulang::effect_checker::EffectChecker;
use nulang::format::constants::LANGUAGE_VERSION_STR;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

#[derive(Debug)]
struct Options {
    source: PathBuf,
    artifact: PathBuf,
    artifact_kind: ArtifactKind,
    package_name: String,
    package_version: String,
    dependency_lock: PathBuf,
    compiler: PathBuf,
    out: PathBuf,
    grants: Vec<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let options = parse_args(std::env::args().skip(1).collect())?;

    let source = std::fs::read(&options.source)
        .map_err(|e| format!("cannot read source '{}': {e}", options.source.display()))?;
    let source_text = std::str::from_utf8(&source)
        .map_err(|e| format!("source '{}' is not UTF-8: {e}", options.source.display()))?;

    let tokens = Lexer::new(source_text)
        .lex()
        .map_err(|e| format!("lex failed: {e}"))?;
    let ast = Parser::new(tokens)
        .parse_module()
        .map_err(|e| format!("parse failed: {e}"))?;

    if contains_import(&ast.decls) {
        return Err(
            "v0alpha1 standalone emission refuses `import`: use a self-contained source file until the emitter is integrated with Nulang's import-aware package frontend"
                .to_string(),
        );
    }

    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .map_err(|e| format!("type check failed: {e}"))?;

    let mut effect_checker = EffectChecker::new();
    // Mirror the normal compiler frontend exactly: even an empty grant list
    // enables the resource-capability gate, causing FS/Net/OS effects to fail
    // closed unless explicitly authorized with --with.
    effect_checker.set_resource_grants(&options.grants);
    effect_checker
        .check_module(&ast.decls)
        .map_err(|e| format!("effect/authority check failed: {e}"))?;

    let artifact_bytes = std::fs::read(&options.artifact)
        .map_err(|e| format!("cannot read artifact '{}': {e}", options.artifact.display()))?;
    let dependency_bytes = std::fs::read(&options.dependency_lock).map_err(|e| {
        format!(
            "cannot read dependency lock '{}': {e}",
            options.dependency_lock.display()
        )
    })?;
    let compiler_bytes = std::fs::read(&options.compiler).map_err(|e| {
        format!(
            "cannot read compiler artifact '{}': {e}",
            options.compiler.display()
        )
    })?;

    let manifest = BehaviorManifest::from_checked_module(
        ManifestBuildInput {
            package_name: &options.package_name,
            package_version: &options.package_version,
            language_version: LANGUAGE_VERSION_STR,
            artifact_kind: options.artifact_kind,
            artifact_bytes: &artifact_bytes,
            compiler_implementation: "nulang-rust",
            compiler_version: env!("CARGO_PKG_VERSION"),
            compiler_bytes: &compiler_bytes,
            source_bytes: &source,
            dependency_bytes: &dependency_bytes,
        },
        &mut effect_checker,
        &ast.decls,
    )
    .map_err(|e| format!("manifest construction failed: {e}"))?;

    let json = manifest
        .to_canonical_json()
        .map_err(|e| format!("manifest serialization failed: {e}"))?;

    if let Some(parent) = options.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create '{}': {e}", parent.display()))?;
        }
    }
    std::fs::write(&options.out, json)
        .map_err(|e| format!("cannot write manifest '{}': {e}", options.out.display()))?;

    println!("{}", options.out.display());
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<Options, String> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        std::process::exit(0);
    }

    let mut source = None;
    let mut artifact = None;
    let mut artifact_kind = None;
    let mut package_name = None;
    let mut package_version = None;
    let mut dependency_lock = None;
    let mut compiler = None;
    let mut out = None;
    let mut grants = Vec::new();

    let mut i = 0usize;
    while i < args.len() {
        let flag = &args[i];
        let value = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };

        match flag.as_str() {
            "--source" => source = Some(PathBuf::from(value(&mut i)?)),
            "--artifact" => artifact = Some(PathBuf::from(value(&mut i)?)),
            "--artifact-kind" => {
                let raw = value(&mut i)?;
                artifact_kind = Some(parse_artifact_kind(&raw)?);
            }
            "--package-name" => package_name = Some(value(&mut i)?),
            "--package-version" => package_version = Some(value(&mut i)?),
            "--dependency-lock" => dependency_lock = Some(PathBuf::from(value(&mut i)?)),
            "--compiler" => compiler = Some(PathBuf::from(value(&mut i)?)),
            "--out" => out = Some(PathBuf::from(value(&mut i)?)),
            "--with" => {
                let raw = value(&mut i)?;
                grants.extend(
                    raw.split(',')
                        .map(str::trim)
                        .filter(|part| !part.is_empty())
                        .map(ToOwned::to_owned),
                );
            }
            other => return Err(format!("unknown option '{other}' (run with --help)")),
        }
        i += 1;
    }

    let source = source.ok_or_else(|| "missing required --source".to_string())?;
    let artifact = artifact.ok_or_else(|| "missing required --artifact".to_string())?;
    let artifact_kind =
        artifact_kind.ok_or_else(|| "missing required --artifact-kind".to_string())?;
    let package_name = package_name.ok_or_else(|| "missing required --package-name".to_string())?;
    let package_version =
        package_version.ok_or_else(|| "missing required --package-version".to_string())?;
    let dependency_lock =
        dependency_lock.ok_or_else(|| "missing required --dependency-lock".to_string())?;
    let compiler = compiler.ok_or_else(|| "missing required --compiler".to_string())?;
    let out = out.unwrap_or_else(|| default_output_path(&artifact));

    Ok(Options {
        source,
        artifact,
        artifact_kind,
        package_name,
        package_version,
        dependency_lock,
        compiler,
        out,
        grants,
    })
}

fn parse_artifact_kind(value: &str) -> Result<ArtifactKind, String> {
    match value {
        "bytecode" => Ok(ArtifactKind::Bytecode),
        "native" => Ok(ArtifactKind::Native),
        "wasm-module" => Ok(ArtifactKind::WasmModule),
        "wasm-component" => Ok(ArtifactKind::WasmComponent),
        other => Err(format!(
            "invalid --artifact-kind '{other}'; expected bytecode|native|wasm-module|wasm-component"
        )),
    }
}

fn default_output_path(artifact: &Path) -> PathBuf {
    let mut name = artifact
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "artifact".into());
    name.push(".behavior.json");
    artifact.with_file_name(name)
}

fn contains_import(decls: &[Decl]) -> bool {
    decls.iter().any(|decl| match decl {
        Decl::Import { .. } => true,
        Decl::Module { decls, .. } => contains_import(decls),
        _ => false,
    })
}

fn print_help() {
    println!(
        "nulang_behavior_manifest (experimental RFC 0020 v0alpha1)\n\
         \n\
         Usage:\n\
           nulang_behavior_manifest \\\n             --source <file.nula> \\\n             --artifact <artifact> \\\n             --artifact-kind <bytecode|native|wasm-module|wasm-component> \\\n             --package-name <name> \\\n             --package-version <version> \\\n             --dependency-lock <Nulang.lock> \\\n             --compiler <compiler-binary> \\\n             [--with <fs,net,os>] \\\n             [--out <manifest.json>]\n\
         \n\
         Security boundary:\n\
           This standalone v0alpha1 emitter refuses source imports and refuses\n\
           missing compiler/dependency provenance rather than emitting an\n\
           incomplete manifest. It emits requirements only and never grants\n\
           runtime authority."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_imports_are_rejected() {
        let source = "module X { import stdlib::web }";
        let ast = Parser::new(Lexer::new(source).lex().unwrap())
            .parse_module()
            .unwrap();
        assert!(contains_import(&ast.decls));
    }

    #[test]
    fn artifact_kind_parser_is_strict() {
        assert_eq!(
            parse_artifact_kind("wasm-module").unwrap(),
            ArtifactKind::WasmModule
        );
        assert!(parse_artifact_kind("wasm").is_err());
    }

    #[test]
    fn required_provenance_inputs_fail_closed() {
        let error = parse_args(vec![
            "--source".into(),
            "main.nula".into(),
            "--artifact".into(),
            "main.wasm".into(),
            "--artifact-kind".into(),
            "wasm-module".into(),
            "--package-name".into(),
            "demo".into(),
            "--package-version".into(),
            "0.1.0".into(),
        ])
        .unwrap_err();
        assert!(error.contains("--dependency-lock"));
    }

    #[test]
    fn output_path_is_adjacent_to_artifact() {
        assert_eq!(
            default_output_path(Path::new("dist/demo.wasm")),
            PathBuf::from("dist/demo.wasm.behavior.json")
        );
    }
}
