use crate::ast::{AstModule, Decl};
use crate::lexer::{Lexer, Token};
use crate::parser::Parser;
use crate::types::{NuError, NuResult, Span};
use std::cell::RefCell;
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

thread_local! {
    /// Fully resolved declarations are only reused within one top-level build.
    /// Parsed ASTs contain process-global fresh TypeVar identities and parser
    /// type lookup can read imports; neither can be reused across builds safely.
    static IMPORT_CACHE: RefCell<BTreeMap<PathBuf, Vec<Decl>>> = const { RefCell::new(BTreeMap::new()) };

    /// Only context-independent lexer tokens survive across builds. Source
    /// content is read and hashed on every request; parsing always runs fresh.
    static TOKEN_CACHE: RefCell<BTreeMap<PathBuf, CachedTokens>> = const { RefCell::new(BTreeMap::new()) };
}

#[cfg(test)]
thread_local! {
    static TOKEN_CACHE_HITS: Cell<usize> = const { Cell::new(0) };
}

const MAX_TOKEN_CACHE_FILES: usize = 64;
const MAX_TOKEN_CACHE_SOURCE_BYTES: usize = 128 * 1024;
const MAX_TOKEN_CACHE_TOKENS: usize = 8192;

struct CachedTokens {
    fingerprint: blake3::Hash,
    tokens: Vec<Token>,
}

/// Reuse context-independent tokens only when the exact source bytes match.
/// Always re-read the file and re-run the parser so imported type definitions
/// and globally fresh parser type variables are never reused incorrectly.
fn lex_import_tokens(path: &Path, source: &str) -> NuResult<Vec<Token>> {
    let eligible = source.len() <= MAX_TOKEN_CACHE_SOURCE_BYTES;
    let fingerprint = blake3::hash(source.as_bytes());
    if eligible {
        let cached = TOKEN_CACHE.with(|cache| {
            cache
                .borrow()
                .get(path)
                .filter(|entry| entry.fingerprint == fingerprint)
                .map(|entry| entry.tokens.clone())
        });
        if let Some(tokens) = cached {
            // Lexer::new installs the source map. Repeat that side effect on
            // hits or later diagnostics can point into an unrelated file.
            let _ = Lexer::new(source);
            #[cfg(test)]
            TOKEN_CACHE_HITS.with(|hits| hits.set(hits.get() + 1));
            return Ok(tokens);
        }
    }

    let tokens = Lexer::new(source).lex()?;
    TOKEN_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if eligible && tokens.len() <= MAX_TOKEN_CACHE_TOKENS {
            if !cache.contains_key(path) && cache.len() >= MAX_TOKEN_CACHE_FILES {
                // Deterministic bounded eviction, no unbounded daemon memory.
                if let Some(oldest_key) = cache.keys().next().cloned() {
                    cache.remove(&oldest_key);
                }
            }
            cache.insert(
                path.to_path_buf(),
                CachedTokens {
                    fingerprint,
                    tokens: tokens.clone(),
                },
            );
        } else {
            cache.remove(path);
        }
    });
    Ok(tokens)
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
            merge_imported_decls(module, filter_decls(cached, items), import_path, true)?;
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
        let tokens = lex_import_tokens(&resolved_canonical, &source)
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

        // Store the full resolved module. Caching a filtered selection under
        // a file-only key would make later imports lose requested symbols.
        let imported_decls = imported.decls;
        IMPORT_CACHE.with(|c| {
            c.borrow_mut()
                .insert(resolved_canonical, imported_decls.clone())
        });
        merge_imported_decls(
            module,
            filter_decls(imported_decls, items),
            import_path,
            false,
        )?;
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

    // Selective imports are represented in the AST even though the current
    // surface parser only constructs unfiltered imports. A file-only cache key
    // must never retain the first import's selection for later requests.
    #[test]
    fn import_cache_does_not_reuse_a_previous_selection() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time is after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nulang-resolver-selective-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).expect("create isolated fixture directory");
        std::fs::write(
            dir.join("dep.nula"),
            "fn alpha() -> Int { 1 }\nfn beta() -> Int { 2 }\n",
        )
        .expect("write fixture source");
        let mut module = AstModule {
            name: "selective_import_regression".into(),
            decls: vec![
                Decl::Import {
                    path: "dep".into(),
                    items: vec!["alpha".into()],
                    span: sp(),
                },
                Decl::Import {
                    path: "dep".into(),
                    items: vec!["beta".into()],
                    span: sp(),
                },
            ],
        };
        let result = resolve_imports(&mut module, &dir.join("main.nula"), &mut HashSet::new());
        std::fs::remove_dir_all(&dir).expect("remove fixture directory");
        result.expect("resolve both selections from the same imported file");
        let names: HashSet<&str> = module.decls.iter().filter_map(decl_name).collect();
        assert!(\n            names.contains("alpha"),\n            "first requested function is present"\n        );
        assert!(\n            names.contains("beta"),\n            "second requested function is present"\n        );
        assert_eq!(names.len(), 2, "imports must not duplicate declarations");
    }

    #[test]
    fn top_level_resolve_imports_discards_stale_declarations() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time is after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nulang-resolver-freshness-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).expect("create isolated fixture directory");
        let dep = dir.join("dep.nula");
        let compile = || {
            let mut module = AstModule {
                name: "freshness".into(),
                decls: vec![Decl::Import {
                    path: "dep".into(),
                    items: vec![],
                    span: sp(),
                }],
            };
            resolve_imports(&mut module, &dir.join("main.nula"), &mut HashSet::new())
                .expect("resolve top-level import");
            module\n                .decls\n                .into_iter()\n                .filter_map(|d| decl_name(&d).map(str::to_string))\n                .collect::<Vec<_>>()
        };
        std::fs::write(&dep, "fn alpha() -> Int { 1 }").expect("write first revision");
        let first = compile();
        std::fs::write(&dep, "fn beta() -> Int { 2 }").expect("write second revision");
        let second = compile();
        std::fs::remove_dir_all(&dir).expect("remove fixture directory");
        assert_eq!(first, vec!["alpha"]);
        assert_eq!(second, vec!["beta"], "a new build must reread the source");
    }

    #[test]
    fn import_lexer_cache_hits_unchanged_sources_and_invalidates_edits() {
        let path = PathBuf::from("lex-cache-regression.nula");
        TOKEN_CACHE.with(|cache| cache.borrow_mut().clear());
        TOKEN_CACHE_HITS.with(|hits| hits.set(0));

        let first = lex_import_tokens(&path, "fn alpha() -> Int { 1 }")
            .expect("lex first version");
        let again = lex_import_tokens(&path, "fn alpha() -> Int { 1 }")
            .expect("lex unchanged version");
        assert_eq!(first, again, "cache must return exactly the original token stream");
        TOKEN_CACHE_HITS.with(|hits| assert_eq!(hits.get(), 1));

        let changed = lex_import_tokens(&path, "fn beta() -> Int { 2 }")
            .expect("lex edited version");
        assert_ne!(first, changed, "edited source must not use stale tokens");
        TOKEN_CACHE_HITS.with(|hits| assert_eq!(hits.get(), 1));

        let cached_changed = lex_import_tokens(&path, "fn beta() -> Int { 2 }")
            .expect("lex unchanged edited source");
        assert_eq!(changed, cached_changed);
        TOKEN_CACHE_HITS.with(|hits| assert_eq!(hits.get(), 2));
        assert_eq!(
            crate::types::current_source_text().as_deref(),
            Some("fn beta() -> Int { 2 }"),
            "a cache hit must update the diagnostic source map just like Lexer::new"
        );
    }

    #[test]
    fn imported_transitive_edit_is_visible_with_parent_tokens_cached() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time is after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nulang-lex-transitive-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).expect("create fixture directory");
        std::fs::write(dir.join("parent.nula"), "import dep\nfn parent() -> Int { 1 }\n")
            .expect("write unchanged parent");
        let dep = dir.join("dep.nula");
        let compile = || {
            let mut module = AstModule {
                name: "transitive".into(),
                decls: vec![Decl::Import {
                    path: "parent".into(),
                    items: vec![],
                    span: sp(),
                }],
            };
            resolve_imports(&mut module, &dir.join("main.nula"), &mut HashSet::new())
                .expect("resolve parent and transitive import");
            module
                .decls
                .iter()
                .filter_map(decl_name)
                .map(str::to_string)
                .collect::<HashSet<_>>()
        };
        TOKEN_CACHE_HITS.with(|hits| hits.set(0));
        std::fs::write(&dep, "fn first() -> Int { 1 }\n").expect("write first dependency");
        let first = compile();
        std::fs::write(&dep, "fn second() -> Int { 2 }\n").expect("edit dependency");
        let second = compile();
        std::fs::remove_dir_all(&dir).expect("remove fixture directory");
        assert!(first.contains("first"));
        assert!(!first.contains("second"));
        assert!(second.contains("second"));
        assert!(!second.contains("first"));
        TOKEN_CACHE_HITS.with(|hits| {
            assert!(hits.get() >= 1, "unchanged parent should reuse lexed tokens")
        });
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
