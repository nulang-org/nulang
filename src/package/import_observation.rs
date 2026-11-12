//! Diagnostic compiler-owned import closure observed by the canonical resolver.
//!
//! This captures the *actual* bytes and paths used by import resolution,
//! including transitive dependencies and resolved standard-library imports.
//! It does not capture the complete compilation recipe (prelude, package and
//! build manifests, compiler flags, backends, type/effect/authority inputs).
//! Consequently, neither this graph nor its ModuleSnapshot is a cache key and
//! nothing here authorizes incremental code reuse.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::AstModule;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::resolver::{self, ResolvedImportTrace};
use crate::types::{NuError, NuResult, Span};

use super::module_invalidation::{ModuleInput, ModuleSnapshot};

#[derive(Debug)]
pub struct ResolvedImportObservation {
    /// The very same AST the canonical resolver produced from supplied source.
    pub module: AstModule,
    /// Only the compiler-observed module source/edge graph. Diagnostic only.
    pub snapshot: ModuleSnapshot,
}

/// Parse the exact root bytes, resolve imports with the normal compiler
/// resolver, and derive a conservative diagnostic module input graph.
///
/// The root file must exist and canonicalize so path identity is not guessed;
/// its supplied source bytes may be an unsaved edit and are compiled exactly
/// as provided. Imported source digests are captured as those files are read
/// by the normal import resolver, rather than by a second filesystem scan.
///
/// `context_bytes` is caller supplied; no assertion of its completeness is
/// made here. This API is deliberately not a production cache interface.
pub fn observe_resolved_imports(
    root: &Path,
    root_source: &[u8],
    context_bytes: &[u8],
) -> NuResult<ResolvedImportObservation> {
    let root_path = root.canonicalize().map_err(|error| NuError::RuntimeError {
        msg: format!(
            "cannot canonicalize import-observation root '{}': {error}",
            root.display()
        ),
        span: Span::default(),
    })?;
    if !root_path.is_file() {
        return Err(NuError::RuntimeError {
            msg: format!(
                "import-observation root is not a file: {}",
                root_path.display()
            ),
            span: Span::default(),
        });
    }
    let source = std::str::from_utf8(root_source).map_err(|error| NuError::RuntimeError {
        msg: format!("root source is not UTF-8: {error}"),
        span: Span::default(),
    })?;
    let tokens = Lexer::new(source).lex()?;
    let mut module = Parser::new(tokens).parse_module()?;

    let mut observed = ResolvedImportTrace::default();
    observed.record_source(&root_path, root_source)?;
    resolver::resolve_imports_traced(&mut module, &root_path, &mut HashSet::new(), &mut observed)?;

    let mut snapshot = ModuleSnapshot::new(context_bytes);
    for (path, source_digest) in &observed.source_digests {
        let name = encode_path(path)?;
        let imports = observed
            .edges
            .get(path)
            .into_iter()
            .flat_map(|edges| edges.iter())
            .map(|import| {
                if !observed.source_digests.contains_key(import) {
                    return Err(NuError::RuntimeError {
                        msg: format!(
                            "resolver import graph missing source for '{}'",
                            import.display()
                        ),
                        span: Span::default(),
                    });
                }
                encode_path(import)
            })
            .collect::<NuResult<BTreeSet<String>>>()?;
        snapshot.modules.insert(
            name,
            ModuleInput {
                source_digest: *source_digest,
                imports,
            },
        );
    }
    Ok(ResolvedImportObservation { module, snapshot })
}

fn encode_path(path: &PathBuf) -> NuResult<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| NuError::RuntimeError {
            msg: format!(
                "non-UTF8 canonical module path cannot be used in a diagnostic snapshot: {}",
                path.display()
            ),
            span: Span::default(),
        })
}
