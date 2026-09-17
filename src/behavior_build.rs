//! Integrated compiler path for RFC 0020 Behavior Manifest emission.
//!
//! Unlike the standalone `nulang_behavior_manifest` prototype, this module
//! resolves imports, checks semantics, lowers, and emits the executable and
//! manifest from one checked compilation unit. It deliberately lives behind
//! the `wasm-backend` feature while v0alpha1 is experimental.

use std::collections::{BTreeSet, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::ast::{AstModule, Decl};
use crate::backends::WasmBackend;
use crate::behavior_manifest::{ArtifactKind, BehaviorManifest, ManifestBuildInput};
use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use crate::format::constants::LANGUAGE_VERSION_STR;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::typechecker::TypeChecker;
use crate::types::{NuError, NuResult, Span};

/// Inputs that identify a package build. The executable bytes are produced by
/// this module; dependency/compiler bytes are supplied by the caller so their
/// provenance is explicit rather than inferred from ambient state.
pub struct BehaviorBuildInput<'a> {
    pub source_path: &'a Path,
    pub package_name: &'a str,
    pub package_version: &'a str,
    pub dependency_bytes: &'a [u8],
    pub compiler_implementation: &'a str,
    pub compiler_version: &'a str,
    pub compiler_bytes: &'a [u8],
    pub with_capabilities: &'a [String],
    pub deny_warnings: bool,
}

pub struct BehaviorBuildOutput {
    pub wasm_bytes: Vec<u8>,
    pub manifest: BehaviorManifest,
}

/// Compile one import-resolved Nulang unit to Wasm and its Behavior Manifest.
///
/// The root source is parsed exactly once for compilation, imports are merged
/// by the ordinary resolver, and the same checked AST/effect checker feeds
/// both MIR/Wasm lowering and manifest construction. A separate provenance
/// walk reads the exact source closure and converts it into a location-neutral
/// content bundle so imported-source changes necessarily change
/// `provenance.source_digest`.
pub fn compile_wasm_behavior(input: BehaviorBuildInput<'_>) -> NuResult<BehaviorBuildOutput> {
    let source_bytes = std::fs::read(input.source_path).map_err(|error| NuError::RuntimeError {
        msg: format!(
            "cannot read source '{}': {error}",
            input.source_path.display()
        ),
        span: Span::default(),
    })?;
    let source = std::str::from_utf8(&source_bytes).map_err(|error| NuError::RuntimeError {
        msg: format!(
            "source '{}' is not UTF-8: {error}",
            input.source_path.display()
        ),
        span: Span::default(),
    })?;

    // Provenance is collected before import declarations are erased by the
    // resolver. The bundle contains sorted content digests, not absolute
    // paths, so identical source closures hash identically on different hosts.
    let source_closure = canonical_source_closure(input.source_path, &source_bytes)?;

    let (ast, type_checker, mut effect_checker) = checked_module(
        source,
        input.source_path,
        input.with_capabilities,
        input.deny_warnings,
    )?;

    let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mir = crate::mir_lower::lower_module(&hir)?;
    let mut wasm_backend = crate::backends::DefaultWasmBackend;
    let wasm_bytes = wasm_backend.compile(&mir, input.package_name)?;

    let manifest = BehaviorManifest::from_checked_module(
        ManifestBuildInput {
            package_name: input.package_name,
            package_version: input.package_version,
            language_version: LANGUAGE_VERSION_STR,
            artifact_kind: ArtifactKind::WasmModule,
            artifact_bytes: &wasm_bytes,
            compiler_implementation: input.compiler_implementation,
            compiler_version: input.compiler_version,
            compiler_bytes: input.compiler_bytes,
            source_bytes: &source_closure,
            dependency_bytes: input.dependency_bytes,
        },
        &mut effect_checker,
        &ast.decls,
    )?;

    Ok(BehaviorBuildOutput {
        wasm_bytes,
        manifest,
    })
}

/// Compiler frontend equivalent to the CLI's `run_frontend`, but returns the
/// successful effect checker as part of the checked compilation unit so
/// manifest emission cannot drift into a source-text or bytecode re-analysis.
fn checked_module(
    source: &str,
    source_path: &Path,
    with_capabilities: &[String],
    deny_warnings: bool,
) -> NuResult<(AstModule, TypeChecker, EffectChecker)> {
    let prelude_source = crate::prelude_source::PRELUDE_SOURCE;
    let mut prelude_lexer = Lexer::new(prelude_source);
    crate::types::set_source_map_with_file(prelude_source, Some("<prelude>"));
    let prelude_tokens = prelude_lexer.lex()?;
    let mut prelude_parser = Parser::new(prelude_tokens);
    let prelude_ast = prelude_parser.parse_module()?;

    let mut lexer = Lexer::new(source);
    crate::types::set_source_map_with_file(source, source_path.to_str());
    let tokens = lexer.lex()?;
    let mut parser = Parser::new(tokens);
    let mut ast = parser.parse_module()?;

    let warnings = parser.take_warnings();
    if !warnings.is_empty() {
        let use_color = std::io::stderr().is_terminal();
        for warning in &warnings {
            eprintln!(
                "{}",
                crate::diagnostic::format_warning(warning, use_color)
            );
        }
        if deny_warnings {
            return Err(NuError::parse_error(
                format!(
                    "aborting due to {} warning{} (--deny-warnings)",
                    warnings.len(),
                    if warnings.len() == 1 { "" } else { "s" }
                ),
                warnings[0].span,
            ));
        }
    }

    let mut prelude_decls: Vec<Decl> = prelude_ast
        .decls
        .into_iter()
        .filter(|decl| matches!(decl, Decl::VariantType { .. }))
        .collect();

    let mut stack = HashSet::new();
    crate::resolver::resolve_imports(&mut ast, source_path, &mut stack)?;

    // Keep the same ordering invariant as the canonical CLI frontend: imported
    // declarations are resolved first, then prelude variants are placed ahead
    // of the merged module so Option/Result constructors are bound in time.
    prelude_decls.append(&mut ast.decls);
    ast.decls = prelude_decls;

    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast)?;

    let flat_decls = crate::effect_checker::flatten_decls(&ast.decls);
    let mut effect_checker = EffectChecker::new();
    effect_checker.set_resource_grants(with_capabilities);
    effect_checker.check_module(&ast.decls)?;
    for message in &effect_checker.diagnostics {
        eprintln!("{message}");
    }

    let route_diagnostics = crate::web::route_check::check_module(&ast);
    for diagnostic in &route_diagnostics {
        eprintln!("route check: {}", diagnostic.message);
    }
    if !route_diagnostics.is_empty() {
        return Err(NuError::TypeError {
            msg: format!(
                "{} route parameter mismatch(es) detected; see diagnostics above",
                route_diagnostics.len()
            ),
            span: Span::default(),
            expected_type: None,
            found_type: None,
            similar_names: None,
        });
    }

    // Reference-capability analysis mirrors the CLI frontend. This is distinct
    // from the external resource-capability gate enforced by EffectChecker.
    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_body = |analyzer: &mut CapabilityAnalyzer,
                    context: &CapContext,
                    body: &crate::ast::Expr|
     -> NuResult<()> { analyzer.infer_cap(context, body).map(|_| ()) };
    let seed_from_params = |context: &mut CapContext, params: &[crate::ast::Param]| {
        for parameter in params {
            if let Some(capability) = parameter.cap {
                *context = context
                    .clone()
                    .with_binding(&parameter.name, capability);
            }
        }
    };

    for decl in flat_decls.iter().copied() {
        match decl {
            Decl::Function { body, params, .. } => {
                let mut context = CapContext::new();
                seed_from_params(&mut context, params);
                cap_body(&mut cap_analyzer, &context, body)?;
            }
            Decl::Actor {
                behaviors,
                state_fields,
                init,
                ..
            } => {
                for behavior in behaviors {
                    let mut context = CapContext::new();
                    seed_from_params(&mut context, &behavior.params);
                    cap_body(&mut cap_analyzer, &context, &behavior.body)?;
                }
                for (_, _, _, default) in state_fields {
                    let context = CapContext::new();
                    cap_body(&mut cap_analyzer, &context, default)?;
                }
                for (_, expression) in init {
                    let context = CapContext::new();
                    cap_body(&mut cap_analyzer, &context, expression)?;
                }
            }
            Decl::Workflow {
                items, compensate, ..
            } => {
                for item in items {
                    let steps: &[crate::ast::WorkflowStep] = match item {
                        crate::ast::WorkflowItem::Step(step) => std::slice::from_ref(step),
                        crate::ast::WorkflowItem::Parallel(steps) => steps,
                    };
                    for step in steps {
                        let context = CapContext::new();
                        cap_body(&mut cap_analyzer, &context, &step.body)?;
                        if let Some(compensation) = &step.compensate {
                            cap_body(&mut cap_analyzer, &context, compensation)?;
                        }
                    }
                }
                if let Some(compensation) = compensate {
                    let context = CapContext::new();
                    cap_body(&mut cap_analyzer, &context, compensation)?;
                }
            }
            _ => {}
        }
    }

    Ok((ast, type_checker, effect_checker))
}

/// Produce a deterministic, location-neutral representation of every source
/// file reachable from the root module. Each file contributes its exact byte
/// digest. The sorted digest set is then hashed by BehaviorManifest, avoiding
/// absolute-path differences between build machines while preserving exact
/// source-content identity.
fn canonical_source_closure(root_path: &Path, root_bytes: &[u8]) -> NuResult<Vec<u8>> {
    let mut visited = BTreeSet::new();
    let mut content_digests = BTreeSet::new();
    collect_source_file(
        root_path,
        Some(root_bytes),
        &mut visited,
        &mut content_digests,
    )?;

    let mut bundle = b"nulang-source-closure/v1\n".to_vec();
    for digest in content_digests {
        bundle.extend_from_slice(digest.as_bytes());
        bundle.push(b'\n');
    }
    Ok(bundle)
}

fn collect_source_file(
    path: &Path,
    supplied_bytes: Option<&[u8]>,
    visited: &mut BTreeSet<PathBuf>,
    content_digests: &mut BTreeSet<String>,
) -> NuResult<()> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }

    let owned;
    let bytes = if let Some(bytes) = supplied_bytes {
        bytes
    } else {
        owned = std::fs::read(&canonical).map_err(|error| NuError::RuntimeError {
            msg: format!(
                "cannot read imported source '{}': {error}",
                canonical.display()
            ),
            span: Span::default(),
        })?;
        &owned
    };

    content_digests.insert(crate::behavior_manifest::digest(bytes));
    let source = std::str::from_utf8(bytes).map_err(|error| NuError::RuntimeError {
        msg: format!(
            "imported source '{}' is not UTF-8: {error}",
            canonical.display()
        ),
        span: Span::default(),
    })?;
    let tokens = Lexer::new(source).lex()?;
    let imported_ast = Parser::new(tokens).parse_module()?;
    let base = canonical.parent().unwrap_or(&canonical);

    for import in imported_ast.decls.iter().filter_map(|decl| match decl {
        Decl::Import { path, .. } => Some(path.as_str()),
        _ => None,
    }) {
        let resolved = resolve_source_path(base, import);
        collect_source_file(&resolved, None, visited, content_digests)?;
    }

    Ok(())
}

/// Mirrors `resolver::resolve_path` for provenance collection. Compilation
/// still uses the real resolver; this helper only finds the same source bytes
/// before import declarations are erased from the AST.
fn resolve_source_path(base: &Path, import: &str) -> PathBuf {
    if let Some(module) = import.strip_prefix("stdlib::") {
        let module_path = module.replace("::", std::path::MAIN_SEPARATOR_STR);
        if let Ok(dir) = std::env::var("NULANG_STDLIB") {
            return PathBuf::from(dir).join(format!("{module_path}.nula"));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let candidate = exe_dir
                    .join("stdlib")
                    .join(format!("{module_path}.nula"));
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            let candidate = cwd
                .join("src")
                .join("stdlib")
                .join(format!("{module_path}.nula"));
            if candidate.exists() {
                return candidate;
            }
        }
        return PathBuf::from(format!("src/stdlib/{module_path}.nula"));
    }

    if let Some(module) = import.strip_prefix("@nulang/") {
        return resolve_nulang_module_path(module);
    }

    let import_path = Path::new(import);
    let resolved = if import_path.is_absolute() {
        import_path.to_path_buf()
    } else {
        base.join(import_path)
    };
    let resolved = if resolved.extension().is_none() {
        resolved.with_extension("nula")
    } else {
        resolved
    };
    if resolved.exists() || import_path.is_absolute() {
        return resolved;
    }

    let mut directory = base;
    loop {
        if directory.join("Nulang.toml").is_file() {
            let candidate = directory.join("src").join(import_path);
            let candidate = if candidate.extension().is_none() {
                candidate.with_extension("nula")
            } else {
                candidate
            };
            if candidate.exists() {
                return candidate;
            }
        }
        match directory.parent() {
            Some(parent) => directory = parent,
            None => break,
        }
    }
    resolved
}

fn resolve_nulang_module_path(module: &str) -> PathBuf {
    let entries = std::env::var("NULANG_MODULE_PATH").unwrap_or_default();
    for entry in entries.split(';').map(str::trim).filter(|entry| !entry.is_empty()) {
        let Some((name, directory)) = entry.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let directory = directory.trim();
        if name.is_empty() || directory.is_empty() {
            continue;
        }
        let name = name.strip_prefix("@nulang/").unwrap_or(name);
        if module == name || module.starts_with(&format!("{name}/")) {
            let rest = module
                .strip_prefix(name)
                .unwrap_or(module)
                .trim_start_matches('/');
            let subpath = if rest.is_empty() {
                PathBuf::from("lib.nula")
            } else {
                PathBuf::from(format!(
                    "{}.nula",
                    rest.replace('/', std::path::MAIN_SEPARATOR_STR)
                ))
            };
            return PathBuf::from(directory).join(subpath);
        }
    }

    PathBuf::from(format!(
        "src/{}.nula",
        module.replace('/', std::path::MAIN_SEPARATOR_STR)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nulang-behavior-build-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn source_closure_digest_changes_when_import_changes() {
        let directory = temp_dir("provenance");
        let main_path = directory.join("main.nula");
        let lib_path = directory.join("lib.nula");
        let root = b"import lib\nfn main() { helper() }\n";
        std::fs::write(&main_path, root).unwrap();
        std::fs::write(&lib_path, "fn helper() { 1 }\n").unwrap();

        let first = canonical_source_closure(&main_path, root).unwrap();
        std::fs::write(&lib_path, "fn helper() { 2 }\n").unwrap();
        let second = canonical_source_closure(&main_path, root).unwrap();

        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn integrated_build_binds_imported_source_and_wasm_bytes() {
        let directory = temp_dir("integrated");
        let main_path = directory.join("main.nula");
        let lib_path = directory.join("lib.nula");
        std::fs::write(&main_path, "import lib\nfn main() { helper() }\n").unwrap();
        std::fs::write(&lib_path, "fn helper() { 7 }\n").unwrap();

        let first = compile_wasm_behavior(BehaviorBuildInput {
            source_path: &main_path,
            package_name: "integrated-test",
            package_version: "0.1.0",
            dependency_bytes: b"lock",
            compiler_implementation: "nulang-rust-test",
            compiler_version: "test",
            compiler_bytes: b"compiler",
            with_capabilities: &[],
            deny_warnings: false,
        })
        .unwrap();

        assert_eq!(
            first.manifest.artifact.digest,
            crate::behavior_manifest::digest(&first.wasm_bytes)
        );

        let first_source_digest = first.manifest.provenance.source_digest.clone();
        std::fs::write(&lib_path, "fn helper() { 8 }\n").unwrap();
        let second = compile_wasm_behavior(BehaviorBuildInput {
            source_path: &main_path,
            package_name: "integrated-test",
            package_version: "0.1.0",
            dependency_bytes: b"lock",
            compiler_implementation: "nulang-rust-test",
            compiler_version: "test",
            compiler_bytes: b"compiler",
            with_capabilities: &[],
            deny_warnings: false,
        })
        .unwrap();

        assert_ne!(first_source_digest, second.manifest.provenance.source_digest);
        let _ = std::fs::remove_dir_all(directory);
    }
}
