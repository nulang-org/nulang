//! Integrated compiler path for RFC 0020 Behavior Manifest emission.
//!
//! Unlike the standalone `nulang_behavior_manifest` prototype, this module
//! resolves imports, checks semantics, lowers, and emits the executable and
//! manifest from one checked compilation unit. It deliberately lives behind
//! the `wasm-backend` feature while v0alpha1 is experimental.

use std::collections::{BTreeSet, HashSet};
use std::io::IsTerminal;
use std::path::Path;

use crate::ast::{AstModule, Decl};
use crate::backends::WasmBackend;
use crate::behavior_manifest::{
    ArtifactKind, BehaviorManifest, HostAbiRequirement, ManifestBuildInput,
};
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
/// The root source is read once. The ordinary resolver returns the exact bytes
/// it consumed for every reachable import, so source provenance is calculated
/// from the same filesystem reads that produced the merged AST. The successful
/// effect checker is then reused for manifest semantics while the same checked
/// AST is lowered to MIR/Wasm.
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

    let (ast, type_checker, mut effect_checker, imported_sources) = checked_module(
        source,
        input.source_path,
        input.with_capabilities,
        input.deny_warnings,
    )?;
    let source_closure = canonical_source_closure(&source_bytes, &imported_sources);

    let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mir = crate::mir_lower::lower_module(&hir)?;
    let host_abi = host_abi_requirement(&mir);
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
            host_abi,
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
/// successful effect checker and the exact imported source bytes as part of the
/// checked compilation unit. Manifest emission therefore never performs a
/// second filesystem/source/bytecode semantic analysis.
fn checked_module(
    source: &str,
    source_path: &Path,
    with_capabilities: &[String],
    deny_warnings: bool,
) -> NuResult<(AstModule, TypeChecker, EffectChecker, Vec<Vec<u8>>)> {
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
            eprintln!("{}", crate::diagnostic::format_warning(warning, use_color));
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
    let imported_sources =
        crate::resolver::resolve_imports_with_sources(&mut ast, source_path, &mut stack)?;

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
                *context = context.clone().with_binding(&parameter.name, capability);
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

    Ok((ast, type_checker, effect_checker, imported_sources))
}

/// Derive the host ABI admission requirement from the exact MIR that is
/// subsequently lowered to Wasm. This intentionally mirrors the Wasm
/// backend's generic-dispatch boundary: IO.print/println/read and Array.length
/// use dedicated imports; every other Perform/PerformAsync crosses the
/// generic host-dispatch seam.
///
/// Known operations are represented only by compiler-owned canonical ids.
/// Unknown/custom operations are not copied into the manifest by source name;
/// instead the manifest records that legacy extension dispatch is still
/// required so canonical-only deployment can fail closed.
fn host_abi_requirement(mir: &crate::mir::Module) -> HostAbiRequirement {
    use crate::mir::{RValue, Stmt};

    let mut required = BTreeSet::new();
    let mut requires_legacy_extension_dispatch = false;

    for function in mir.functions.iter().chain(mir.behaviors.iter()) {
        for block in &function.blocks {
            for stmt in &block.stmts {
                let Stmt::Assign { op, .. } = stmt else {
                    continue;
                };

                let source_operation = match op {
                    RValue::Perform { effect, op, .. } => Some((effect.as_str(), op.as_str())),
                    RValue::PerformAsync { effect_op, .. } => {
                        let (effect, op) = effect_op
                            .split_once('.')
                            .unwrap_or((effect_op.as_str(), ""));
                        Some((effect, op))
                    }
                    _ => None,
                };

                let Some((effect, operation)) = source_operation else {
                    continue;
                };

                if matches!(
                    (effect, operation),
                    ("IO", "print")
                        | ("IO", "println")
                        | ("IO", "read")
                        | ("Array", "length")
                ) {
                    continue;
                }

                if let Some(descriptor) =
                    crate::host_effect_abi::lookup_host_operation(effect, operation)
                {
                    required.insert(descriptor.canonical_id());
                } else {
                    requires_legacy_extension_dispatch = true;
                }
            }
        }
    }

    HostAbiRequirement {
        schema: crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
        required_operations: required.into_iter().collect(),
        requires_legacy_extension_dispatch,
    }
}

/// Produce a deterministic, location-neutral representation of the exact root
/// and imported source bytes consumed by the compiler. The imported sources
/// come directly from the resolver's parse reads; no file is opened again.
fn canonical_source_closure(root_bytes: &[u8], imported_sources: &[Vec<u8>]) -> Vec<u8> {
    let mut content_digests = BTreeSet::new();
    content_digests.insert(crate::behavior_manifest::digest(root_bytes));
    for source in imported_sources {
        content_digests.insert(crate::behavior_manifest::digest(source));
    }

    let mut bundle = b"nulang-source-closure/v1\n".to_vec();
    for digest in content_digests {
        bundle.extend_from_slice(digest.as_bytes());
        bundle.push(b'\n');
    }
    bundle
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
    fn source_closure_is_content_bound_and_order_independent() {
        let first =
            canonical_source_closure(b"root", &[b"import-a".to_vec(), b"import-b".to_vec()]);
        let reordered =
            canonical_source_closure(b"root", &[b"import-b".to_vec(), b"import-a".to_vec()]);
        let changed =
            canonical_source_closure(b"root", &[b"import-a".to_vec(), b"import-c".to_vec()]);

        assert_eq!(first, reordered);
        assert_ne!(first, changed);
    }

    #[test]
    fn host_abi_requirement_uses_canonical_ids_without_source_spellings() {
        let directory = temp_dir("host-abi");
        let main_path = directory.join("main.nula");
        std::fs::write(
            &main_path,
            "perform Storage.write(\"key\", \"value\")\n",
        )
        .unwrap();

        let source = std::fs::read_to_string(&main_path).unwrap();
        let (ast, type_checker, _, _) =
            checked_module(&source, &main_path, &[], false).unwrap();
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mir = crate::mir_lower::lower_module(&hir).unwrap();
        let requirement = host_abi_requirement(&mir);

        assert_eq!(
            requirement.required_operations,
            vec![
                "nulang.host-effects/v0alpha1:nulang:storage/string#Write".to_string()
            ]
        );
        assert!(!requirement.requires_legacy_extension_dispatch);
        assert!(
            requirement
                .required_operations
                .iter()
                .all(|operation| !operation.contains("Storage.write"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn unknown_host_dispatch_is_marked_legacy_without_leaking_source_name() {
        let directory = temp_dir("legacy-host-abi");
        let main_path = directory.join("main.nula");
        std::fs::write(
            &main_path,
            "effect Custom { ping: String -> String }\nperform Custom.ping(\"x\")\n",
        )
        .unwrap();

        let source = std::fs::read_to_string(&main_path).unwrap();
        let (ast, type_checker, _, _) =
            checked_module(&source, &main_path, &[], false).unwrap();
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mir = crate::mir_lower::lower_module(&hir).unwrap();
        let requirement = host_abi_requirement(&mir);

        assert!(requirement.required_operations.is_empty());
        assert!(requirement.requires_legacy_extension_dispatch);

        let serialized = serde_json::to_string(&requirement).unwrap();
        assert!(!serialized.contains("Custom.ping"));

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

        assert_ne!(
            first_source_digest,
            second.manifest.provenance.source_digest
        );
        let _ = std::fs::remove_dir_all(directory);
    }
}
