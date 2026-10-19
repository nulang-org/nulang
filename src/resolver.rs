use crate::ast::{AstModule, Decl};
use crate::lexer::Lexer;
use crate::parser::Parser;
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
    if stack.is_empty() {
        IMPORT_CACHE.with(|c| c.borrow_mut().clear());
    }

    let canonical_file = current_file
        .canonicalize()
        .unwrap_or_else(|_| current_file.to_path_buf());
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
        let resolved_canonical = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());

        // If the file has already been fully resolved in this compilation,
        // reuse its cached declarations.
        if let Some(cached) = IMPORT_CACHE.with(|c| c.borrow().get(&resolved_canonical).cloned()) {
            // Cache the *full* source module, not a previous importer's selection.
            let selected = select_import_decls(cached, items, import_path)?;
            merge_imported_decls(module, selected, import_path, true)?;
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
        let mut imported =
            Parser::new(tokens)
                .parse_module()
                .map_err(|e| NuError::RuntimeError {
                    msg: format!("parse error in '{}': {}", import_path, e),
                    span: Span::default(),
                })?;

        resolve_imports(&mut imported, &resolved_canonical, stack)?;

        // Store unfiltered exports. Multiple imports of the same module may
        // request disjoint subsets, including on a cache hit.
        let all_decls = imported.decls;
        IMPORT_CACHE.with(|c| {
            c.borrow_mut()
                .insert(resolved_canonical, all_decls.clone())
        });
        let selected = select_import_decls(all_decls, items, import_path)?;
        merge_imported_decls(module, selected, import_path, false)?;
    }
    module.decls.retain(|d| !matches!(d, Decl::Import { .. }));
    stack.remove(&canonical_file);
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
    // stdlib::set → resolve to STDLIB_DIR/set.nula or dev fallback.
    // stdlib::web::types → STDLIB_DIR/web/types.nula.
    if let Some(module) = import.strip_prefix("stdlib::") {
        let module_path = module.replace("::", std::path::MAIN_SEPARATOR_STR);
        // Try NULANG_STDLIB env var first
        if let Ok(dir) = std::env::var("NULANG_STDLIB") {
            return PathBuf::from(dir).join(format!("{}.nula", module_path));
        }
        // Try relative to executable
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let candidate = exe_dir.join("stdlib").join(format!("{}.nula", module_path));
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        // Development fallback: src/stdlib/ relative to CWD
        if let Ok(cwd) = std::env::current_dir() {
            let dev_path = cwd
                .join("src")
                .join("stdlib")
                .join(format!("{}.nula", module_path));
            if dev_path.exists() {
                return dev_path;
            }
        }
        // Last resort: relative to CWD
        return PathBuf::from(format!("src/stdlib/{}.nula", module_path));
    }

    // @nulang/auth or @nulang/auth/session → resolved via NULANG_MODULE_PATH.
    if let Some(module) = import.strip_prefix("@nulang/") {
        return resolve_module_path(module);
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

/// Fail rather than silently omitting misspelled names from a selective import.
fn select_import_decls(
    decls: Vec<Decl>,
    items: &[String],
    import_path: &str,
) -> NuResult<Vec<Decl>> {
    if items.is_empty() {
        return Ok(decls);
    }
    let available: HashSet<&str> = decls.iter().filter_map(decl_name).collect();
    if let Some(missing) = items.iter().find(|item| !available.contains(item.as_str())) {
        return Err(NuError::RuntimeError {
            msg: format!("import '{}' has no declaration named '{}'", import_path, missing),
            span: Span::default(),
        });
    }
    // Import resolution currently flattens declarations into the caller. It
    // cannot keep a private dependency closure: silently omitting a sibling
    // helper or a type would create a broken program. Until module namespaces
    // exist, allow only selections that are demonstrably self-contained.
    let selected: HashSet<&str> = items.iter().map(String::as_str).collect();
    let module_names: HashSet<&str> = decls.iter().filter_map(decl_name).collect();

    // Type references may occur in signatures, constructors, and patterns;
    // this pass does not yet track them all. Reject partial type namespaces
    // rather than allowing a type-correctness bug to escape.
    if let Some(name) = decls.iter().find_map(|decl| {
        if matches!(decl, Decl::TypeAlias { .. } | Decl::RecordType { .. } | Decl::VariantType { .. })
            && decl_name(decl).is_some_and(|n| !selected.contains(n))
        {
            decl_name(decl)
        } else {
            None
        }
    }) {
        return Err(NuError::RuntimeError {
            msg: format!(
                "selective import from '{}' would omit type '{}'; import the complete module until namespace dependencies are supported",
                import_path, name
            ),
            span: Span::default(),
        });
    }

    for decl in &decls {
        let Some(name) = decl_name(decl) else {
            // Other declarations (including module-level lets, instances,
            // effects and module declarations) may provide necessary state
            // or authority. Restrict selection to independent function/type
            // modules until their linkage is modeled explicitly.
            return Err(NuError::RuntimeError {
                msg: format!(
                    "selective import from '{}' cannot safely omit non-function declarations; import the complete module",
                    import_path
                ),
                span: Span::default(),
            });
        };
        if !selected.contains(name) {
            continue;
        }
        if let Decl::Function {
            body, params, using_params, default_values, requires, ensures, ..
        } = decl
        {
            let mut bound: Vec<String> = params.iter().chain(using_params.iter())
                .map(|p| p.name.clone()).collect();
            let mut referenced = Vec::new();
            crate::effect_checker::free_vars(body, &mut bound, &mut referenced);
            for default in default_values.iter().flatten() {
                crate::effect_checker::free_vars(default, &mut bound, &mut referenced);
            }
            for contract in requires.iter().chain(ensures.iter()) {
                crate::effect_checker::free_vars(contract, &mut bound, &mut referenced);
            }
            if let Some(missing) = referenced.iter().find(|candidate| {
                module_names.contains(candidate.as_str())
                    && !selected.contains(candidate.as_str())
            }) {
                return Err(NuError::RuntimeError {
                    msg: format!(
                        "selective import of '{}' from '{}' omits required module declaration '{}'; import it too or import the whole module",
                        name, import_path, missing
                    ),
                    span: Span::default(),
                });
            }
        }
    }
    Ok(filter_decls(decls, items))
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

/// Resolve an `@nulang/<module>` import path using the `NULANG_MODULE_PATH`
/// environment variable. Entries are semicolon-separated `name=src_dir` pairs.
/// The package source directory is expected to contain the module's `.nula`
/// files; the bare import maps to `<dir>/lib.nula`, and subpaths map to
/// `<dir>/<subpath>.nula`.
fn resolve_module_path(module: &str) -> PathBuf {
    let entries = std::env::var("NULANG_MODULE_PATH").unwrap_or_default();
    let mut chosen_dir: Option<PathBuf> = None;
    let mut chosen_prefix = String::new();
    for entry in entries.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((name, dir)) = entry.split_once('=') {
            let name = name.trim();
            let dir = dir.trim();
            if name.is_empty() || dir.is_empty() {
                continue;
            }
            // Accept both `auth=...` and `@nulang/auth=...` in the env var.
            let name = name.strip_prefix("@nulang/").unwrap_or(name);
            if module == name || module.starts_with(&format!("{}/", name)) {
                chosen_dir = Some(PathBuf::from(dir));
                chosen_prefix = name.to_string();
                break;
            }
        }
    }
    if let Some(dir) = chosen_dir {
        let rest = module.strip_prefix(&chosen_prefix).unwrap_or(module);
        let rest = rest.trim_start_matches('/');
        let subpath = if rest.is_empty() {
            "lib.nula"
        } else {
            &format!("{}.nula", rest.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        return dir.join(subpath);
    }
    // Fallback that produces a deterministic error path when the env var is
    // not set or does not contain the requested module.
    PathBuf::from(format!(
        "src/{}.nula",
        module.replace('/', std::path::MAIN_SEPARATOR_STR)
    ))
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
    #[test]
    fn test_resolve_cached_import_applies_each_selection() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("nulang-import-test-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("helpers.nula");
        std::fs::write(&source, "fn alpha() = 1\nfn beta() = 2\n").unwrap();
        let mut module = AstModule {
            name: "test".into(),
            decls: vec![
                Decl::Import { path: "helpers".into(), items: vec!["alpha".into()], span: sp() },
                Decl::Import { path: "helpers".into(), items: vec!["beta".into()], span: sp() },
            ],
        };
        let result = resolve_imports(&mut module, &dir.join("main.nula"), &mut HashSet::new());
        let _ = std::fs::remove_dir_all(&dir);
        result.expect("both names should be imported after a cache hit");
        assert_eq!(module.decls.iter().filter(|d| decl_name(d) == Some("alpha")).count(), 1);
        assert_eq!(module.decls.iter().filter(|d| decl_name(d) == Some("beta")).count(), 1);
    }

    #[test]
    fn test_selective_import_rejects_omitted_function_dependency() {
        let mut parser = Parser::new(
            Lexer::new("fn abs(x: Int) = if x < 0 then -x else x\nfn gcd(a: Int, b: Int) = abs(a)")
                .lex()
                .expect("valid source"),
        );
        let decls = parser.parse_module().expect("parse test module").decls;
        let result = select_import_decls(decls, &["gcd".into()], "math");
        assert!(result.is_err(), "selecting gcd must not silently discard abs");
    }

    #[test]
    fn test_selective_import_accepts_standalone_function() {
        let mut parser = Parser::new(
            Lexer::new("fn abs(x: Int) = if x < 0 then -x else x\nfn gcd(a: Int, b: Int) = abs(a)")
                .lex()
                .expect("valid source"),
        );
        let decls = parser.parse_module().expect("parse test module").decls;
        let selected = select_import_decls(decls, &["abs".into()], "math")
            .expect("independent function should be selectable");
        assert_eq!(selected.len(), 1);
        assert_eq!(decl_name(&selected[0]), Some("abs"));
    }

    #[test]
    fn test_selective_import_rejects_omitted_module_type() {
        let mut parser = Parser::new(
            Lexer::new("type Id = Int\nfn use_id(id: Id) -> Id = id")
                .lex()
                .expect("valid source"),
        );
        let decls = parser.parse_module().expect("parse test module").decls;
        let result = select_import_decls(decls, &["use_id".into()], "types");
        assert!(result.is_err(), "type dependencies must not silently disappear");
    }

    #[test]
    fn test_resolve_selective_import_rejects_unknown_name() {
        let decls = vec![fn_decl("alpha")];
        let result = select_import_decls(decls, &["missing".into()], "helpers");
        assert!(result.is_err(), "unknown import names must not be ignored");
    }

}
