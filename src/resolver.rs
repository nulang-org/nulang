use crate::ast::{AstModule, Decl};
use crate::lexer::Lexer;
use crate::package::module_invalidation::ModuleSnapshot;
use crate::parser::{Parser, ParserReadTrace};
use crate::types::{NuError, NuResult, Span};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

thread_local! {
    /// Cache of fully resolved import declarations keyed by canonical file path.
    /// Cleared at the start of every top-level `resolve_imports` call so that
    /// repeated compilations in the same process do not reuse stale ASTs.
    static IMPORT_CACHE: RefCell<BTreeMap<PathBuf, Vec<Decl>>> = const { RefCell::new(BTreeMap::new()) };
}

pub fn resolve_imports(
    module: &mut AstModule,
    current_file: &Path,
    stack: &mut HashSet<PathBuf>,
) -> NuResult<()> {
    resolve_imports_inner(module, current_file, stack, None, "", None)
}

/// Parse and resolve a source string while recording the files and direct edges
/// observed by the recursive import resolver. This is diagnostic only:
/// successful parser-side imported-type reads are checked against the observed
/// graph, but failed probes, prelude, toolchain, environment and other compiler
/// inputs are not proven complete. Never use this snapshot to skip
/// compilation or reuse artifacts. Canonical path identities are local to this
/// machine, not portable cache keys. The root digest describes the supplied
/// source bytes, not a second read of the entry file.
pub fn resolve_source_with_snapshot(
    source: &str,
    current_file: &Path,
    context_bytes: &[u8],
) -> NuResult<(AstModule, ModuleSnapshot)> {
    let trace = ParserReadTrace::default();
    let mut module = Parser::new(Lexer::new(source).lex()?)
        .with_read_trace(trace.clone())
        .parse_module()?;
    let mut snapshot = ModuleSnapshot::new(context_bytes);
    resolve_imports_inner(
        &mut module,
        current_file,
        &mut HashSet::new(),
        Some(&mut snapshot),
        source,
        Some(&trace),
    )?;
    verify_parser_reads(&snapshot, &trace.lock().expect("parser trace lock"))?;
    Ok((module, snapshot))
}

fn verify_parser_reads(snapshot: &ModuleSnapshot, reads: &[(PathBuf, String)]) -> NuResult<()> {
    for (path, source) in reads {
        let canonical = canonical_path(path, true)?;
        let name = snapshot_name(&canonical)?;
        if !snapshot
            .modules
            .get(&name)
            .is_some_and(|input| input.source_digest == *blake3::hash(source.as_bytes()).as_bytes())
        {
            return Err(NuError::RuntimeError {
                msg: format!("parser read is absent or differs from resolved snapshot: {name}"),
                span: Span::default(),
            });
        }
    }
    Ok(())
}

fn canonical_path(path: &Path, tracing: bool) -> NuResult<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if tracing => Err(NuError::RuntimeError {
            msg: format!(
                "cannot identify snapshot path '{}': {error}",
                path.display()
            ),
            span: Span::default(),
        }),
        Err(_) => Ok(path.to_path_buf()),
    }
}

fn snapshot_name(path: &Path) -> NuResult<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| NuError::RuntimeError {
            msg: "module snapshot path is not valid UTF-8".into(),
            span: Span::default(),
        })
}

fn resolve_imports_inner(
    module: &mut AstModule,
    current_file: &Path,
    stack: &mut HashSet<PathBuf>,
    mut snapshot: Option<&mut ModuleSnapshot>,
    source: &str,
    read_trace: Option<&ParserReadTrace>,
) -> NuResult<()> {
    if stack.is_empty() {
        IMPORT_CACHE.with(|c| c.borrow_mut().clear());
    }

    let canonical_file = canonical_path(current_file, snapshot.is_some())?;
    let mut observed_imports = Vec::new();
    if stack.contains(&canonical_file) {
        return Err(NuError::RuntimeError {
            msg: format!("import cycle detected at '{}'", current_file.display()),
            span: Span::default(),
        });
    }
    stack.insert(canonical_file.clone());
    let canonical_base = canonical_file
        .parent()
        .unwrap_or(&canonical_file)
        .to_path_buf();

    let imports: Vec<(String, Vec<String>)> = module
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Import { path, items, .. } => Some((path.clone(), items.clone())),
            _ => None,
        })
        .collect();

    for (import_path, items) in &imports {
        let resolved = resolve_path(&canonical_base, import_path);
        let resolved_canonical = canonical_path(&resolved, snapshot.is_some())?;
        // Record each edge before the per-compilation cache lookup, so shared
        // imports in diamonds are visible from every importing module.
        if snapshot.is_some() {
            observed_imports.push(snapshot_name(&resolved_canonical)?);
        }

        // If the file has already been fully resolved in this compilation,
        // reuse its cached declarations.
        if let Some(cached) = IMPORT_CACHE.with(|c| c.borrow().get(&resolved_canonical).cloned()) {
            merge_imported_decls(module, cached, import_path, true)?;
            continue;
        }

        // Cycle detection: a file still on the recursion stack cannot be imported.
        if stack.contains(&resolved_canonical) {
            return Err(NuError::RuntimeError {
                msg: format!("import cycle detected at '{}'", import_path),
                span: Span::default(),
            });
        }

        let source = std::fs::read_to_string(&resolved).map_err(|e| NuError::RuntimeError {
            msg: format!("cannot read '{}': {}", import_path, e),
            span: Span::default(),
        })?;
        let tokens = Lexer::new(&source)
            .lex()
            .map_err(|e| NuError::RuntimeError {
                msg: format!("lex error in '{}': {}", import_path, e),
                span: Span::default(),
            })?;
        let mut parser = Parser::new(tokens);
        if let Some(trace) = read_trace {
            parser = parser.with_read_trace(trace.clone());
        }
        let mut imported = parser.parse_module().map_err(|e| NuError::RuntimeError {
            msg: format!("parse error in '{}': {}", import_path, e),
            span: Span::default(),
        })?;

        resolve_imports_inner(
            &mut imported,
            &resolved_canonical,
            stack,
            snapshot.as_deref_mut(),
            &source,
            read_trace,
        )?;

        let imported_decls = if items.is_empty() {
            imported.decls
        } else {
            filter_decls(imported.decls, items)
        };

        IMPORT_CACHE.with(|c| {
            c.borrow_mut()
                .insert(resolved_canonical, imported_decls.clone())
        });
        merge_imported_decls(module, imported_decls, import_path, false)?;
    }
    module.decls.retain(|d| !matches!(d, Decl::Import { .. }));
    stack.remove(&canonical_file);
    if let Some(snapshot) = snapshot {
        snapshot.insert(
            snapshot_name(&canonical_file)?,
            source.as_bytes(),
            observed_imports,
        );
    }
    Ok(())
}

fn merge_imported_decls(
    module: &mut AstModule,
    imported_decls: Vec<Decl>,
    import_path: &str,
    from_cache: bool,
) -> NuResult<()> {
    // Reject name collisions between what THIS import brings in and
    // what's already present (from an earlier import or the importing
    // file itself) up front, with a clear diagnostic pointing at the
    // conflicting import path. Without this check, two stdlib modules
    // that both define e.g. `empty`/`contains`/`remove` (map.nula and
    // set.nula both do) silently produce two top-level `Decl::Function`
    // entries with the same name; MIR's function-slot allocator isn't
    // built to handle that and fails much later with an opaque
    // "internal: MIR function slot 0 left unfilled" — a compiler crash
    // for user-visible input, not a real internal invariant violation.
    //
    // When `from_cache` is true we are reusing a previously-resolved module
    // (e.g., `stdlib::web::types` imported transitively by `host` after
    // already being imported directly). Duplicates in that case are almost
    // always shared stdlib dependencies, so we silently skip them rather than
    // produce a collision for every shared import.
    //
    // Type declarations (aliases, records, variants) are also allowed to
    // collide: they are often re-exported through multiple stdlib modules
    // (e.g., `Html` from `stdlib::web::types` is imported by both `html`
    // and `host`). Keeping the first occurrence is sufficient because
    // identical nominal types from the same source are indistinguishable.
    fn is_type_decl(decl: &Decl) -> bool {
        matches!(
            decl,
            Decl::TypeAlias { .. } | Decl::RecordType { .. } | Decl::VariantType { .. }
        )
    }
    let existing_names: HashSet<&str> = module.decls.iter().filter_map(decl_name).collect();
    let existing_type_decl = |name: &str| {
        module
            .decls
            .iter()
            .find(|d| decl_name(d) == Some(name))
            .map_or(false, is_type_decl)
    };
    let mut filtered = Vec::new();
    for decl in imported_decls {
        if let Some(name) = decl_name(&decl) {
            if existing_names.contains(name) {
                if from_cache || (is_type_decl(&decl) && existing_type_decl(name)) {
                    continue;
                }
                return Err(NuError::RuntimeError {
                    msg: format!(
                        "import conflict: '{}' from '{}' collides with an already-imported \
                         or locally-declared name of the same name (qualified/aliased \
                         imports aren't available). Fix by renaming one of the conflicting \
                         declarations, or importing only the names you need with \
                         `import {} {{ specific_name }}`.",
                        name, import_path, import_path
                    ),
                    span: Span::default(),
                });
            }
        }
        filtered.push(decl);
    }

    let mut merged = filtered;
    merged.append(&mut module.decls);
    module.decls = merged;
    Ok(())
}

fn resolve_path(base: &Path, import: &str) -> PathBuf {
    if let Some(path) = crate::import_paths::resolve_named(import) {
        return path;
    }
    // Preserve the historical resolver fallback for unconfigured packages.
    if let Some(module) = import.strip_prefix("@nulang/") {
        return PathBuf::from(format!(
            "src/{}.nula",
            module.replace('/', std::path::MAIN_SEPARATOR_STR)
        ));
    }

    let p = Path::new(import);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let resolved = if resolved.extension().is_none() {
        resolved.with_extension("nula")
    } else {
        resolved
    };
    if resolved.exists() || p.is_absolute() {
        return resolved;
    }
    // Package-aware fallback: a bare module import (e.g. `import lib`) that
    // isn't a sibling of the importing file is looked up in the package's
    // `src/` directory. The package root is found by walking up from the
    // importing file to the nearest directory containing a `Nulang.toml`.
    // This is what lets `tests/*.nula` files import the package's own
    // modules (e.g. `import lib` → `src/lib.nula`) when run by `nula test`.
    let mut dir = base;
    loop {
        if dir.join("Nulang.toml").is_file() {
            let candidate = dir.join("src").join(p);
            let candidate = if candidate.extension().is_none() {
                candidate.with_extension("nula")
            } else {
                candidate
            };
            if candidate.exists() {
                return candidate;
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    resolved
}

fn decl_name(decl: &Decl) -> Option<&str> {
    match decl {
        Decl::Function { name, .. }
        | Decl::Actor { name, .. }
        | Decl::StateMachine { name, .. }
        | Decl::TypeAlias { name, .. }
        | Decl::RecordType { name, .. }
        | Decl::VariantType { name, .. }
        | Decl::EffectDecl { name, .. }
        | Decl::Module { name, .. }
        | Decl::Agent { name, .. }
        | Decl::Database { name, .. } => Some(name.as_str()),
        Decl::NamedHandler { name, .. } => Some(name.as_str()),
        Decl::CrdtDecl { name, .. } => Some(name.as_str()),
        Decl::Extern { .. }
        | Decl::Workflow { .. }
        | Decl::Import { .. }
        | Decl::Class { .. }
        | Decl::LetBinding { .. }
        | Decl::Signal { .. }
        | Decl::Impl { .. }
        | Decl::Given { .. } => None,
    }
}

fn filter_decls(decls: Vec<Decl>, items: &[String]) -> Vec<Decl> {
    if items.is_empty() {
        return decls;
    }
    let set: HashSet<&str> = items.iter().map(|s| s.as_str()).collect();
    decls
        .into_iter()
        .filter(|d| decl_name(d).map_or(false, |n| set.contains(n)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Literal};
    use crate::types::Span;
    fn sp() -> Span {
        Span::new(0, 0)
    }
    fn fn_decl(name: &str) -> Decl {
        Decl::Function {
            name: name.into(),
            type_params: vec![],
            type_param_constraints: vec![],
            params: vec![],
            default_values: vec![],
            using_params: vec![],
            ret_type: None,
            error_type: None,
            effect: None,
            cap: None,
            requires: vec![],
            ensures: vec![],
            body: Expr::Literal(Literal::Int(0), sp()),
            annotations: vec![],
            public: false,
            span: sp(),
        }
    }

    struct SnapshotFixture(PathBuf);

    impl SnapshotFixture {
        fn new() -> Self {
            static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "nulang-snapshot-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn write(&self, name: &str, source: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, source).unwrap();
            path
        }

        fn key(&self, name: &str) -> String {
            self.0
                .join(name)
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        }
    }

    impl Drop for SnapshotFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolved_snapshot_captures_transitive_sources_and_cached_edges() {
        let root = SnapshotFixture::new();
        let source = "import left\nimport right\nfn main() { 0 }";
        let entry = root.write("main.nula", source);
        root.write("left.nula", "import shared\nfn left() { 1 }");
        root.write("right.nula", "import shared\nfn right() { 2 }");
        // Identical re-exported type declarations are permitted by the existing
        // resolver; shared function names are currently rejected on this diamond.
        let shared = "type Shared = First | Second";
        root.write("shared.nula", shared);
        let (ast, snapshot) = resolve_source_with_snapshot(source, &entry, b"context").unwrap();
        let mut normal = Parser::new(Lexer::new(source).lex().unwrap())
            .parse_module()
            .unwrap();
        resolve_imports(&mut normal, &entry, &mut HashSet::new()).unwrap();
        assert_eq!(ast, normal);
        assert_eq!(snapshot.modules.len(), 4);
        assert_eq!(
            snapshot.modules[&root.key("main.nula")].source_digest,
            *blake3::hash(source.as_bytes()).as_bytes()
        );
        assert_eq!(
            snapshot.modules[&root.key("shared.nula")].source_digest,
            *blake3::hash(shared.as_bytes()).as_bytes()
        );
        for name in ["left.nula", "right.nula"] {
            assert_eq!(
                snapshot.modules[&root.key(name)].imports,
                [root.key("shared.nula")].into_iter().collect()
            );
        }
        assert_eq!(
            snapshot.modules[&root.key("main.nula")].imports,
            [root.key("left.nula"), root.key("right.nula")]
                .into_iter()
                .collect()
        );

        root.write("shared.nula", "type Shared = First | Third");
        let (_, updated) = resolve_source_with_snapshot(source, &entry, b"context").unwrap();
        use crate::package::module_invalidation::explain_invalidation;
        let impact = explain_invalidation(&snapshot, &updated);
        assert_eq!(impact.changed_modules, vec![root.key("shared.nula")]);
        assert_eq!(
            impact.affected_in_dependency_order,
            vec![
                root.key("shared.nula"),
                root.key("left.nula"),
                root.key("right.nula"),
                root.key("main.nula")
            ]
        );
    }

    #[test]
    fn imported_type_lookup_is_traced_and_matches_resolver_snapshot() {
        let root = SnapshotFixture::new();
        let source = "import stdlib::web::types\nfn identity_html(x: Html) -> Html { x }";
        let entry = root.write("main.nula", source);
        let trace = ParserReadTrace::default();
        Parser::new(Lexer::new(source).lex().unwrap())
            .with_read_trace(trace.clone())
            .parse_module()
            .unwrap();
        let reads = trace.lock().unwrap();
        assert!(
            !reads.is_empty(),
            "fixture must actually read an imported type"
        );
        let (_, snapshot) = resolve_source_with_snapshot(source, &entry, b"context").unwrap();
        assert!(verify_parser_reads(&snapshot, &reads).is_ok());
        assert!(snapshot.modules.len() >= 2);
    }

    #[test]
    fn parser_read_verification_rejects_untracked_or_changed_bytes() {
        let root = SnapshotFixture::new();
        let path = root.write("types.nula", "type Sample = First | Second");
        let mut snapshot = ModuleSnapshot::new(b"context");
        let reads = vec![(path.clone(), "type Sample = First | Second".to_owned())];
        assert!(verify_parser_reads(&snapshot, &reads).is_err());
        snapshot.insert(
            root.key("types.nula"),
            reads[0].1.as_bytes(),
            [] as [&str; 0],
        );
        assert!(verify_parser_reads(&snapshot, &reads).is_ok());
        let changed = vec![(path, "type Sample = First | Third".to_owned())];
        assert!(verify_parser_reads(&snapshot, &changed).is_err());
    }

    #[test]
    fn resolved_snapshot_refuses_missing_import() {
        let root = SnapshotFixture::new();
        let source = "import missing\nfn main() { 0 }";
        let entry = root.write("main.nula", source);
        assert!(resolve_source_with_snapshot(source, &entry, b"context").is_err());
    }

    #[test]
    fn resolved_snapshot_refuses_import_cycle() {
        let root = SnapshotFixture::new();
        let source = "import other\nfn main() { 0 }";
        let entry = root.write("main.nula", source);
        root.write("other.nula", "import main\nfn other() { 1 }");
        assert!(resolve_source_with_snapshot(source, &entry, b"context").is_err());
    }

    #[test]
    fn test_filter_empty() {
        let decls = vec![fn_decl("f")];
        assert_eq!(filter_decls(decls, &[]).len(), 1);
    }

    #[test]
    fn test_filter_names() {
        let r = filter_decls(vec![fn_decl("a"), fn_decl("b")], &["a".into()]);
        assert_eq!(r.len(), 1);
        assert_eq!(decl_name(&r[0]), Some("a"));
    }

    #[test]
    fn test_resolve_module_path() {
        std::env::set_var("NULANG_MODULE_PATH", "@nulang/auth=/tmp/nulang_auth_mod");
        let base = std::path::Path::new(".");
        assert_eq!(
            resolve_path(base, "@nulang/auth"),
            std::path::PathBuf::from("/tmp/nulang_auth_mod/lib.nula")
        );
        assert_eq!(
            resolve_path(base, "@nulang/auth/session"),
            std::path::PathBuf::from("/tmp/nulang_auth_mod/session.nula")
        );
        std::env::remove_var("NULANG_MODULE_PATH");
    }
}
