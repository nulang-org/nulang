use crate::ast::{AstModule, Decl};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::types::{NuError, NuResult, Span};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Declaration provenance is resolver-only metadata. Never encode filesystem
/// paths into runtime types, bytecode, or persistent schema identities.
#[derive(Clone)]
struct SourcedDecl {
    decl: Decl,
    origin: PathBuf,
}

thread_local! {
    /// Cache *complete* resolved modules, with the original file for each
    /// declaration. Selective imports are filtered at the use site, not cached
    /// under a path-wide key (which would lose subsequent selected names).
    /// Cleared at the start of each independent resolution.
    static IMPORT_CACHE: RefCell<BTreeMap<PathBuf, Vec<SourcedDecl>>> = const { RefCell::new(BTreeMap::new()) };
}

pub fn resolve_imports(
    module: &mut AstModule,
    current_file: &Path,
    stack: &mut HashSet<PathBuf>,
) -> NuResult<()> {
    resolve_imports_with_origins(module, current_file, stack).map(|_| ())
}

fn resolve_imports_with_origins(
    module: &mut AstModule,
    current_file: &Path,
    stack: &mut HashSet<PathBuf>,
) -> NuResult<Vec<SourcedDecl>> {
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

    // Original declarations belong to this module; recursively imported
    // declarations keep their defining module's identity, even through
    // reexports or repeated imports.
    let mut resolved: Vec<SourcedDecl> = module
        .decls
        .iter()
        .cloned()
        .map(|decl| SourcedDecl {
            decl,
            origin: canonical_file.clone(),
        })
        .collect();

    for (import_path, items) in &imports {
        let path = resolve_path(&canonical_base, import_path);
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.clone());

        // A cached module is complete; selections are specific to each import.
        if let Some(cached) = IMPORT_CACHE.with(|c| c.borrow().get(&canonical_path).cloned()) {
            merge_imported_decls(&mut resolved, filter_decls(cached, items), import_path)?;
            continue;
        }

        if stack.contains(&canonical_path) {
            return Err(NuError::RuntimeError {
                msg: format!("import cycle detected at '{}'", import_path),
                span: Span::default(),
            });
        }

        let source = std::fs::read_to_string(&path).map_err(|e| NuError::RuntimeError {
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

        let full = resolve_imports_with_origins(&mut imported, &canonical_path, stack)?;
        IMPORT_CACHE.with(|c| {
            c.borrow_mut().insert(canonical_path, full.clone());
        });
        merge_imported_decls(&mut resolved, filter_decls(full, items), import_path)?;
    }

    resolved.retain(|entry| !matches!(entry.decl, Decl::Import { .. }));
    module.decls = resolved.iter().map(|entry| entry.decl.clone()).collect();
    stack.remove(&canonical_file);
    Ok(resolved)
}

fn merge_imported_decls(
    module: &mut Vec<SourcedDecl>,
    imported_decls: Vec<SourcedDecl>,
    import_path: &str,
) -> NuResult<()> {
    // A repeated import/reexport is safe only if the declaration has the
    // *same defining source*. Same short name or same underlying type is not
    // sufficient: they may represent unrelated opaque types.
    let existing: BTreeMap<String, PathBuf> = module
        .iter()
        .filter_map(|entry| {
            decl_name(&entry.decl).map(|name| (name.to_string(), entry.origin.clone()))
        })
        .collect();
    let mut filtered = Vec::new();
    for entry in imported_decls {
        if let Some(name) = decl_name(&entry.decl) {
            if let Some(previous_origin) = existing.get(name) {
                if previous_origin == &entry.origin {
                    continue;
                }
                return Err(NuError::RuntimeError {
                    msg: format!(
                        "import conflict: '{}' from '{}' is defined in '{}', but the same name \
                         already refers to a declaration in '{}'. Rename one declaration or \
                         import only the symbols you need (qualified aliases are not supported).",
                        name,
                        import_path,
                        entry.origin.display(),
                        previous_origin.display(),
                    ),
                    span: Span::default(),
                });
            }
        }
        // Preserve multiple declarations in one import payload. Collision
        // checking across separate imports happens at the module boundary.
        filtered.push(entry);
    }

    let mut merged = filtered;
    merged.append(module);
    *module = merged;
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

fn filter_decls(decls: Vec<SourcedDecl>, items: &[String]) -> Vec<SourcedDecl> {
    if items.is_empty() {
        return decls;
    }
    let set: HashSet<&str> = items.iter().map(|s| s.as_str()).collect();
    decls
        .into_iter()
        .filter(|entry| decl_name(&entry.decl).is_some_and(|name| set.contains(name)))
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
        let decls = vec![SourcedDecl {
            decl: fn_decl("f"),
            origin: PathBuf::from("local.nula"),
        }];
        assert_eq!(filter_decls(decls, &[]).len(), 1);
    }

    #[test]
    fn test_filter_names() {
        let r = filter_decls(
            vec![
                SourcedDecl {
                    decl: fn_decl("a"),
                    origin: PathBuf::from("a.nula"),
                },
                SourcedDecl {
                    decl: fn_decl("b"),
                    origin: PathBuf::from("b.nula"),
                },
            ],
            &["a".into()],
        );
        assert_eq!(r.len(), 1);
        assert_eq!(decl_name(&r[0].decl), Some("a"));
    }

    #[test]
    fn declarations_from_one_import_keep_their_original_multiplicity() {
        // The resolver must not silently drop two same-named declarations
        // supplied by one file; later phases decide whether they are valid.
        let origin = PathBuf::from("overloads.nula");
        let mut module = Vec::new();
        merge_imported_decls(
            &mut module,
            vec![
                SourcedDecl {
                    decl: fn_decl("handler"),
                    origin: origin.clone(),
                },
                SourcedDecl {
                    decl: fn_decl("handler"),
                    origin,
                },
            ],
            "overloads",
        )
        .unwrap();
        assert_eq!(module.len(), 2);
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

    // Import identity must be about the declaration's origin, not its short
    // name or structurally identical source text.
    fn resolve_fixture(main_source: &str, imported: &[(&str, &str)]) -> NuResult<AstModule> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        let n = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nulang_nominal_import_{}_{}", std::process::id(), n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let main_file = dir.join("main.nula");
        std::fs::write(&main_file, main_source).unwrap();
        for (name, contents) in imported {
            std::fs::write(dir.join(format!("{name}.nula")), contents).unwrap();
        }
        let tokens = Lexer::new(main_source).lex().unwrap();
        let mut module = Parser::new(tokens).parse_module().unwrap();
        let result = resolve_imports(&mut module, &main_file, &mut HashSet::new())
            .map(|()| module);
        std::fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn independent_opaque_types_with_same_name_must_conflict() {
        let err = resolve_fixture(
            "import first\nimport second\n",
            &[
                ("first", "opaque type Token = Int\n"),
                ("second", "opaque type Token = Int\n"),
            ],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("import conflict"));
        assert!(format!("{err}").contains("Token"));
    }

    #[test]
    fn imported_type_cannot_silently_shadow_local_type() {
        let err = resolve_fixture(
            "opaque type Token = Int\nimport foreign\n",
            &[("foreign", "opaque type Token = Int\n")],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("import conflict"));
    }

    #[test]
    fn repeated_import_of_same_type_origin_is_idempotent() {
        let module = resolve_fixture(
            "import shared\nimport shared\n",
            &[("shared", "opaque type Token = Int\n")],
        )
        .unwrap();
        assert_eq!(
            module.decls.iter().filter(|d| decl_name(d) == Some("Token")).count(),
            1
        );
    }

    #[test]
    fn transitive_reexport_of_same_type_origin_is_idempotent() {
        let module = resolve_fixture(
            "import shared\nimport wrapper\n",
            &[
                ("shared", "opaque type Token = Int\n"),
                ("wrapper", "import shared\n"),
            ],
        )
        .unwrap();
        assert_eq!(
            module.decls.iter().filter(|d| decl_name(d) == Some("Token")).count(),
            1
        );
    }
}
