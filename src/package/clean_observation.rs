//! Hermetic observation fixture for the incremental correctness oracle.
//!
//! Intentionally limited to independently compiled, standalone pure-function
//! modules. It has no imports, cache integration, deployment authority, or
//! production compiler hook. All outputs come from Nulang's actual compiler
//! components; source-level guesses are never used as output fingerprints.
//!
//! This is *not* the general CLI's complete input collector. In particular,
//! stdlib/prelude resolution, arbitrary module graphs, actors, workflows,
//! resource grants, backend selection and failed-compilation diagnostics still
//! require a compiler-owned collector before any production reuse is possible.

use std::collections::BTreeMap;

use crate::ast::Decl;
use crate::effect_checker::{flatten_decls, CapContext, CapabilityAnalyzer, EffectChecker};
use crate::hir_lower;
use crate::lexer::Lexer;
use crate::mir_codegen;
use crate::mir_lower;
use crate::parser::Parser;
use crate::semantic_identity::canonical_mir_bytes;
use crate::typechecker::TypeChecker;

use super::incremental_oracle::{BuildObservation, CompilerOutput};
use super::module_invalidation::ModuleSnapshot;

/// A complete *standalone test fixture*, not a production compilation closure.
/// Both the snapshot and the observation are created in one compiler invocation.
#[derive(Debug)]
pub struct CleanObservations {
    pub snapshot: ModuleSnapshot,
    pub build: BuildObservation,
}

/// Recompile all independent sources in the supplied fixture from scratch.
///
/// Only standalone functions are supported, and modules may not import one
/// another (or any standard library module). Rejects unknown inputs rather
/// than accepting a potentially incomplete dependency graph. No caching,
/// incremental execution, filesystem inputs, or external effects take place.
pub fn observe_unlinked_bytecode_sources<'a, I>(sources: I) -> Result<CleanObservations, String>
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let sources: Vec<_> = sources.into_iter().collect();
    if sources.is_empty() {
        return Err("standalone observation requires at least one module".into());
    }

    // This recipe describes only this hermetic diagnostic fixture. It is not
    // a build/cache key and must not be used for any source in production.
    let context = format!(
        "nulang.clean-standalone-bytecode-observation.v1;compiler={};target={}-{};profile=no-imports-pure-functions",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    let mut snapshot = ModuleSnapshot::new(context.as_bytes());
    let mut outputs = BTreeMap::new();

    for (name, bytes) in sources {
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(format!("invalid standalone module name '{name}'"));
        }
        if outputs.contains_key(name) {
            return Err(format!("duplicate standalone module name '{name}'"));
        }
        let source = std::str::from_utf8(bytes)
            .map_err(|error| format!("non-UTF-8 standalone module '{name}': {error}"))?;
        let output = compile_standalone(name, source)?;
        snapshot.insert(name, bytes, std::iter::empty::<&str>());
        outputs.insert(name.to_owned(), output);
    }

    let mut build = BuildObservation::new(&snapshot);
    build.modules = outputs;
    Ok(CleanObservations { snapshot, build })
}

fn compile_standalone(name: &str, source: &str) -> Result<CompilerOutput, String> {
    let tokens = Lexer::new(source)
        .lex()
        .map_err(|e| format!("{name}: lex failed: {e}"))?;
    let mut parser = Parser::new(tokens);
    let ast = parser
        .parse_module()
        .map_err(|e| format!("{name}: parse failed: {e}"))?;
    let parser_warnings = parser.take_warnings();
    let flat = flatten_decls(&ast.decls);
    // This intentionally rejects every source declaration outside our small
    // hermetic profile, even when the general compiler could compile it.
    for decl in &flat {
        if !matches!(decl, Decl::Function { .. }) {
            return Err(format!(
                "{name}: unsupported declaration (including import/effect/actor);                  standalone observation has no compiler-owned dependency closure"
            ));
        }
    }

    let mut effects = EffectChecker::new();
    effects.set_resource_grants(&[]);
    effects
        .check_module(&ast.decls)
        .map_err(|e| format!("{name}: effect check failed: {e}"))?;
    let effect_rows = effects.function_rows();
    for (function, row) in &effect_rows {
        if !row.effects().is_empty() {
            return Err(format!(
                "{name}: unsupported effectful function '{function}' in pure observation profile"
            ));
        }
    }

    let mut tc = TypeChecker::new();
    tc.check_module(&ast)
        .map_err(|e| format!("{name}: type check failed: {e}"))?;

    // Match the production frontend's capability check for supported
    // function declarations, including annotated parameter capabilities.
    let mut caps = CapabilityAnalyzer::new();
    for decl in flat {
        if let Decl::Function { body, params, .. } = decl {
            let mut ctx = CapContext::new();
            for param in params {
                if let Some(cap) = param.cap {
                    ctx = ctx.with_binding(&param.name, cap);
                }
            }
            caps.infer_cap(&ctx, body)
                .map_err(|e| format!("{name}: capability check failed: {e}"))?;
        }
    }

    let route_diagnostics = crate::web::route_check::check_module(&ast);
    if !route_diagnostics.is_empty() {
        return Err(format!("{name}: unsupported web route diagnostics"));
    }

    // These are compiler-owned type/effect facts, encoded in deterministic
    // name order; never hash FxHashMap's iteration order.
    let mut type_facts = BTreeMap::new();
    for (name, ty) in &tc.inferred_decl_types {
        type_facts.insert(name.clone(), format!("{ty:?}"));
    }
    let mut row_facts = BTreeMap::new();
    for (name, row) in effect_rows {
        row_facts.insert(name, format!("{row:?}"));
    }
    let typed_bytes = format!("types={type_facts:?};effects={row_facts:?}");

    let hir = hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir =
        mir_lower::lower_module(&hir).map_err(|e| format!("{name}: MIR lowering failed: {e}"))?;
    let mir_bytes = canonical_mir_bytes(&mir)
        .map_err(|e| format!("{name}: MIR canonicalization failed: {e}"))?;
    let module = mir_codegen::compile_mir(&mut mir, name)
        .map_err(|e| format!("{name}: bytecode compile failed: {e}"))?;
    let artifact = module
        .to_nbc(Some(*blake3::hash(source.as_bytes()).as_bytes()))
        .map_err(|e| format!("{name}: bytecode serialization failed: {e}"))?;

    let mut diagnostics: Vec<String> = parser_warnings
        .iter()
        .map(|warning| format!("{warning:?}"))
        .collect();
    diagnostics.extend(effects.diagnostics.iter().cloned());
    // Stable ordering in the diagnostic stream; no non-fatal messages are
    // silently dropped. Unsupported failing compilations produce an Err.
    let diag_bytes = diagnostics.join("\n");
    Ok(CompilerOutput::from_bytes(
        diag_bytes.as_bytes(),
        Some(typed_bytes.as_bytes()),
        Some(&mir_bytes),
        Some(&artifact),
    ))
}
