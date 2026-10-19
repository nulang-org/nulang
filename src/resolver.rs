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

/// A qualified symbol's lexical category; no runtime or artifact encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualifiedSymbolKind {
    Function,
    Type,
}

/// Source-level identity of one exported declaration. The canonical source
/// path is resolver metadata only: do not embed it in serialized type IDs,
/// compiled NBC artifacts, or distributed actor protocols.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifiedSymbol {
    pub origin: PathBuf,
    pub name: String,
    pub kind: QualifiedSymbolKind,
}

/// A single module's visible exports. Keep overloaded declarations as a list,
/// rather than silently choosing the last definition with a given name.
#[derive(Clone, Debug)]
struct IndexedModule {
    origin: PathBuf,
    exports: BTreeMap<String, Vec<QualifiedSymbol>>,
}

/// Lexically scoped alias lookup foundation for future qualified imports.
///
/// This index deliberately does *not* lower or execute aliases. The existing
/// frontend still rejects imported aliases until all compiler passes use
/// resolved symbols consistently. Only explicitly public declarations are
/// indexed. Reexports and constructors require a later export-model pass.
#[derive(Debug, Default)]
pub struct QualifiedImportIndex {
    aliases: BTreeMap<String, IndexedModule>,
}

impl QualifiedImportIndex {
    /// Register an already-parsed module under a local alias.
    ///
    /// Repeat registration of the same canonical source is idempotent;
    /// attempting to reuse an alias for another source fails closed.
    pub fn bind(
        &mut self,
        alias: &str,
        source_file: &Path,
        module: &AstModule,
        span: Span,
    ) -> NuResult<()> {
        if alias.is_empty() {
            return Err(NuError::parse_error(
                "empty qualified module alias".to_string(),
                span,
            ));
        }
        let origin = source_file.canonicalize().map_err(|error| {
            NuError::parse_error(
                format!(
                    "cannot resolve canonical module source '{}': {}",
                    source_file.display(),
                    error
                ),
                span,
            )
        })?;
        if let Some(existing) = self.aliases.get(alias) {
            if existing.origin == origin {
                return Ok(());
            }
            return Err(NuError::parse_error(
                format!(
                    "module alias conflict: '{}' already refers to a different module",
                    alias
                ),
                span,
            ));
        }
        let mut exports: BTreeMap<String, Vec<QualifiedSymbol>> = BTreeMap::new();
        for decl in &module.decls {
            let Some((name, kind)) = exported_decl(decl) else {
                continue;
            };
            exports
                .entry(name.to_string())
                .or_default()
                .push(QualifiedSymbol {
                    origin: origin.clone(),
                    name: name.to_string(),
                    kind,
                });
        }
        self.aliases
            .insert(alias.to_string(), IndexedModule { origin, exports });
        Ok(())
    }

    /// Resolve a public symbol without falling back to unrelated unqualified
    /// bindings. Overloads are preserved for subsequent type-directed lookup.
    pub fn resolve(
        &self,
        alias: &str,
        symbol: &str,
        span: Span,
    ) -> NuResult<&[QualifiedSymbol]> {
        let module = self.aliases.get(alias).ok_or_else(|| {
            NuError::parse_error(format!("unknown module alias '{}'", alias), span)
        })?;
        module
            .exports
            .get(symbol)
            .map(Vec::as_slice)
            .ok_or_else(|| {
                NuError::parse_error(
                    format!("'{}' is not exported by module alias '{}'", symbol, alias),
                    span,
                )
            })
    }
}

/// Conservative export discovery: declarations without explicit visibility
/// are not accidentally made public by import aliases.
fn exported_decl(decl: &Decl) -> Option<(&str, QualifiedSymbolKind)> {
    match decl {
        Decl::Function {
            name,
            public: true,
            ..
        } => Some((name, QualifiedSymbolKind::Function)),
        Decl::TypeAlias {
            name,
            public: true,
            ..
        }
        | Decl::RecordType {
            name,
            public: true,
            ..
        }
        | Decl::VariantType {
            name,
            public: true,
            ..
        } => Some((name, QualifiedSymbolKind::Type)),
        _ => None,
    }
}

/// Build a resolver-only index of direct aliased imports. This is a
/// preparatory semantic pass: the compiler still rejects alias imports until
/// the full type/HIR/backend pipeline consumes resolved symbols.
///
/// This pass does not recursively reexport dependencies or flatten imported
/// declarations into the requesting module. Missing or private exports
/// remain inaccessible; callers cannot fall back to unrelated local names.
pub fn index_qualified_imports(
    module: &AstModule,
    current_file: &Path,
) -> NuResult<QualifiedImportIndex> {
    let base = current_file
        .canonicalize()
        .unwrap_or_else(|_| current_file.to_path_buf());
    let dir = base.parent().unwrap_or(&base);
    let mut index = QualifiedImportIndex::default();

    for declaration in &module.decls {
        let Decl::Import {
            path,
            alias: Some(alias),
            items,
            span,
        } = declaration
        else {
            continue;
        };
        if module.decls.iter().any(|decl| {
            !matches!(decl, Decl::Import { .. }) && decl_name(decl) == Some(alias.as_str())
        }) {
            return Err(NuError::parse_error(
                format!("module alias shadows local declaration: '{}'", alias),
                *span,
            ));
        }
        if !items.is_empty() {
            return Err(NuError::parse_error(
                "qualified imports with a selective item list are not supported".to_string(),
                *span,
            ));
        }
        let path = resolve_path(dir, path);
        let source = std::fs::read_to_string(&path).map_err(|error| {
            NuError::parse_error(
                format!("cannot read qualified module '{}': {}", path.display(), error),
                *span,
            )
        })?;
        let tokens = Lexer::new(&source).lex().map_err(|error| {
            NuError::parse_error(
                format!("lex error in qualified module '{}': {}", path.display(), error),
                *span,
            )
        })?;
        let imported = Parser::new(tokens).parse_module().map_err(|error| {
            NuError::parse_error(
                format!("parse error in qualified module '{}': {}", path.display(), error),
                *span,
            )
        })?;
        index.bind(alias, &path, &imported, *span)?;
    }
    Ok(index)
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

    // Until qualified symbol lookup exists, never treat `import X as alias`
    // like a legacy import: flattening would silently leak wrong bindings.
    if let Some((alias, span)) = module.decls.iter().find_map(|decl| match decl {
        Decl::Import {
            alias: Some(alias),
            span,
            ..
        } => Some((alias.as_str(), *span)),
        _ => None,
    }) {
        return Err(NuError::parse_error(
            format!(
                "qualified imports are not yet supported by module resolution (alias '{}'); use an unqualified import or wait for namespace resolution",
                alias
            ),
            span,
        ));
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

    resolved.retain(|entry| !matches!(&entry.decl, Decl::Import { .. }));
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
        | Decl::Database { name, .. }
        | Decl::NamedHandler { name, .. }
        | Decl::CrdtDecl { name, .. }
        | Decl::Workflow { name, .. }
        | Decl::Class { name, .. }
        | Decl::LetBinding { name, .. }
        | Decl::Signal { name, .. }
        | Decl::Given { name, .. } => Some(name.as_str()),
        // Extern blocks and instance impls do not declare a single symbol.
        Decl::Extern { .. } | Decl::Import { .. } | Decl::Impl { .. } => None,
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
    fn named_declaration_kinds_participate_in_import_identity() {
        let span = sp();
        let zero = Expr::Literal(Literal::Int(0), span);
        let declarations = vec![
            Decl::LetBinding {
                name: "settings".into(),
                type_ann: None,
                value: zero.clone(),
                mutable: false,
                span,
            },
            Decl::Signal {
                name: "status".into(),
                ty: crate::types::Type::int(),
                init: zero.clone(),
                span,
            },
            Decl::Given {
                name: "request".into(),
                ty: Some(crate::types::Type::int()),
                value: zero,
                span,
            },
            Decl::Workflow {
                name: "checkout".into(),
                input: None,
                items: vec![],
                compensate: None,
                span,
            },
            Decl::Class {
                name: "Serializable".into(),
                type_params: vec![],
                type_param_constraints: vec![],
                super_classes: vec![],
                methods: vec![],
                span,
            },
        ];
        let actual: Vec<_> = declarations.iter().map(decl_name).collect();
        assert_eq!(
            actual,
            vec![
                Some("settings"),
                Some("status"),
                Some("request"),
                Some("checkout"),
                Some("Serializable"),
            ]
        );
    }

    #[test]
    fn imports_cannot_silently_shadow_a_local_value_binding() {
        let err = resolve_fixture(
            "let token = 1\nimport foreign\n",
            &[("foreign", "let token = 2\n")],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("import conflict"), "{err}");
        assert!(format!("{err}").contains("token"), "{err}");
    }

    #[test]
    fn independent_imported_value_bindings_with_same_name_conflict() {
        let err = resolve_fixture(
            "import first\nimport second\n",
            &[("first", "let config = 1\n"), ("second", "let config = 2\n")],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("import conflict"), "{err}");
        assert!(format!("{err}").contains("config"), "{err}");
    }

    #[test]
    fn repeated_value_binding_import_is_idempotent() {
        let module = resolve_fixture(
            "import shared\nimport shared\n",
            &[("shared", "let config = 1\n")],
        )
        .unwrap();
        assert_eq!(
            module
                .decls
                .iter()
                .filter(|decl| decl_name(decl) == Some("config"))
                .count(),
            1
        );
    }

    #[test]
    fn unresolved_import_alias_fails_closed_before_loading_module() {
        // A parser accepting the syntax must not accidentally resolve this
        // as an ordinary unqualified import before namespace binding exists.
        let err = resolve_fixture("import foreign as source\n", &[]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("qualified imports"), "{msg}");
        assert!(!msg.contains("cannot read"), "{msg}");
    }

    fn public_fn(name: &str) -> Decl {
        let mut decl = fn_decl(name);
        if let Decl::Function { public, .. } = &mut decl {
            *public = true;
        }
        decl
    }

    fn temp_module_path(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_SYMBOL_FIXTURE: AtomicU64 = AtomicU64::new(0);
        let n = NEXT_SYMBOL_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nulang_symbol_index_{}_{}_{}.nula",
            std::process::id(),
            n,
            label
        ));
        std::fs::write(&path, "").expect("create test module");
        path
    }

    #[test]
    fn qualified_symbols_from_separate_modules_remain_distinct() {
        let left = temp_module_path("left");
        let right = temp_module_path("right");
        let ast = AstModule {
            name: "test".into(),
            decls: vec![public_fn("run")],
        };
        let mut bindings = QualifiedImportIndex::default();
        bindings.bind("one", &left, &ast, sp()).expect("bind first");
        bindings.bind("two", &right, &ast, sp()).expect("bind second");
        let first = bindings.resolve("one", "run", sp()).expect("first exported");
        let second = bindings.resolve("two", "run", sp()).expect("second exported");
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].origin, second[0].origin);
        assert_eq!(first[0].kind, QualifiedSymbolKind::Function);
        std::fs::remove_file(left).unwrap();
        std::fs::remove_file(right).unwrap();
    }

    #[test]
    fn qualified_index_does_not_export_private_declarations() {
        let path = temp_module_path("visibility");
        let ast = AstModule {
            name: "test".into(),
            decls: vec![fn_decl("private"), public_fn("exposed")],
        };
        let mut bindings = QualifiedImportIndex::default();
        bindings.bind("mod", &path, &ast, sp()).expect("bind");
        assert!(bindings.resolve("mod", "exposed", sp()).is_ok());
        let err = bindings.resolve("mod", "private", sp()).unwrap_err();
        assert!(format!("{err}").contains("not exported"), "{err}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn qualified_index_rejects_unknown_alias_or_missing_export() {
        let path = temp_module_path("unknown");
        let ast = AstModule {
            name: "test".into(),
            decls: vec![public_fn("known")],
        };
        let mut bindings = QualifiedImportIndex::default();
        bindings.bind("mod", &path, &ast, sp()).expect("bind");
        let unknown = bindings.resolve("elsewhere", "known", sp()).unwrap_err();
        let missing = bindings.resolve("mod", "missing", sp()).unwrap_err();
        assert!(format!("{unknown}").contains("unknown module alias"));
        assert!(format!("{missing}").contains("not exported"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn duplicate_alias_same_source_is_idempotent_but_different_source_fails() {
        let first = temp_module_path("first");
        let second = temp_module_path("second");
        let ast = AstModule {
            name: "test".into(),
            decls: vec![public_fn("go")],
        };
        let mut bindings = QualifiedImportIndex::default();
        bindings.bind("lib", &first, &ast, sp()).unwrap();
        bindings.bind("lib", &first, &ast, sp()).unwrap();
        assert_eq!(bindings.resolve("lib", "go", sp()).unwrap().len(), 1);
        let err = bindings.bind("lib", &second, &ast, sp()).unwrap_err();
        assert!(format!("{err}").contains("module alias conflict"), "{err}");
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
    }

    #[test]
    fn public_opaque_types_keep_canonical_source_origin() {
        let path = temp_module_path("opaque");
        let mut bindings = QualifiedImportIndex::default();
        let ast = AstModule {
            name: "test".into(),
            decls: vec![Decl::TypeAlias {
                name: "Token".into(),
                type_params: vec![],
                body: crate::types::Type::int(),
                opaque: true,
                public: true,
                span: sp(),
            }],
        };
        bindings.bind("ids", &path, &ast, sp()).unwrap();
        let symbols = bindings.resolve("ids", "Token", sp()).unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].kind, QualifiedSymbolKind::Type);
        assert_eq!(symbols[0].origin, path.canonicalize().unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn source_import_index_discovers_public_symbols_without_flattening() {
        let temp = temp_module_path("root");
        let sibling = temp.with_file_name(format!(
            "nulang_qualified_source_{}_{}.nula",
            std::process::id(),
            temp.file_stem().unwrap().to_string_lossy()
        ));
        std::fs::write(
            &sibling,
            "pub fn accessible() -> Int { 7 }\nfn internal() -> Int { 8 }\n",
        )
        .unwrap();
        let source = format!(
            "import {} as utilities\n",
            sibling.file_stem().unwrap().to_string_lossy()
        );
        let tokens = Lexer::new(&source).lex().unwrap();
        let module = Parser::new(tokens).parse_module().unwrap();
        let index = index_qualified_imports(&module, &temp).expect("read qualified module");
        assert!(index.resolve("utilities", "accessible", sp()).is_ok());
        assert!(index.resolve("utilities", "internal", sp()).is_err());
        // Indexing must not modify or flatten the program's import AST.
        assert!(matches!(&module.decls[0], Decl::Import { alias: Some(a), .. } if a == "utilities"));
        std::fs::remove_file(temp).unwrap();
        std::fs::remove_file(sibling).unwrap();
    }

    #[test]
    fn source_import_index_rejects_alias_shadowing_local_names() {
        let path = temp_module_path("alias_collision");
        let module = AstModule {
            name: "test".into(),
            decls: vec![
                public_fn("utilities"),
                Decl::Import {
                    path: "missing".into(),
                    items: vec![],
                    alias: Some("utilities".into()),
                    span: sp(),
                },
            ],
        };
        let err = index_qualified_imports(&module, &path).unwrap_err();
        assert!(format!("{err}").contains("alias shadows local declaration"), "{err}");
        std::fs::remove_file(path).unwrap();
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
