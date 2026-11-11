//! Whole-program differential proof against real compiler import resolution.
//!
//! The proposed snapshot is observational only: it does not allow reuse of
//! typechecking, MIR, executable artifacts, or any shared cache entries.
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::effect_checker::EffectChecker;
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::package::module_invalidation::{invalidated_modules, Invalidation, ModuleSnapshot};
use nulang::parser::Parser;
use nulang::resolver::resolve_imports_with_snapshot;
use nulang::typechecker::TypeChecker;
use nulang::types::NuError;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nulang-resolved-parity-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).expect("create private test workspace");
        let ws = Self { dir };
        ws.write("core", "fn core() -> Int { 1 }\n");
        ws.write("api", "import core\nfn api() -> Int { core() + 1 }\n");
        ws.write("main", "import api\nfn entry() -> Int { api() }\n");
        ws
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.nula"))
    }

    fn write(&self, name: &str, content: &str) {
        fs::write(self.file(name), content).expect("write workspace source");
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SemanticObservation {
    types: Vec<String>,
    bytecode: Vec<u32>,
    constants: Vec<String>,
    effect_sites: Vec<String>,
}

fn error_code(error: NuError) -> String {
    format!("{}: {error}", error.stable_code().unwrap_or("unknown"))
}

fn compile_resolved(
    root: &Path,
    context: &[u8],
) -> Result<(ModuleSnapshot, SemanticObservation), String> {
    let source = fs::read_to_string(root).expect("root source must exist");
    let tokens = Lexer::new(&source).lex().map_err(error_code)?;
    let mut ast = Parser::new(tokens).parse_module().map_err(error_code)?;
    let mut stack = HashSet::new();
    let snapshot =
        resolve_imports_with_snapshot(&mut ast, root, &mut stack, source.as_bytes(), context)
            .map_err(error_code)?;

    let mut checker = TypeChecker::new();
    checker.check_module(&ast).map_err(error_code)?;
    EffectChecker::new()
        .check_module(&ast.decls)
        .map_err(error_code)?;

    let mut types: Vec<_> = checker
        .inferred_decl_types
        .iter()
        .map(|(name, typ)| format!("{name}: {typ:?}"))
        .collect();
    types.sort();

    let hir = hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).map_err(error_code)?;
    let compiled = mir_codegen::compile_mir(&mut mir, "resolved-parity").map_err(error_code)?;
    Ok((
        snapshot,
        SemanticObservation {
            types,
            bytecode: compiled
                .instructions
                .iter()
                .map(|instruction| instruction.encode())
                .collect(),
            constants: compiled
                .constants
                .iter()
                .map(|constant| format!("{constant:?}"))
                .collect(),
            effect_sites: compiled
                .effect_sites
                .iter()
                .map(|site| format!("{site:?}"))
                .collect(),
        },
    ))
}

#[test]
fn real_resolver_edges_invalidate_importing_modules_and_match_clean_compile() {
    let ws = Workspace::new();
    let (before, initial) = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();

    let main = ws
        .file("main")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let api = ws
        .file("api")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let core = ws
        .file("core")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(before.modules.len(), 3);
    assert!(before.modules[&main].imports.contains(&api));
    assert!(before.modules[&api].imports.contains(&core));
    assert!(before.modules[&core].imports.is_empty());

    ws.write("core", "fn core() -> Int { 42 }\n");
    let (after, changed) = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();
    assert_eq!(
        invalidated_modules(&before, &after),
        Invalidation::Dirty {
            modules: vec![api, core, main]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        }
    );
    assert_ne!(
        initial, changed,
        "source body edit must affect compiled program"
    );
    assert_eq!(
        compile_resolved(&ws.file("main"), b"compiler-v1")
            .unwrap()
            .1,
        changed
    );
}

#[test]
fn interface_error_is_reported_by_clean_whole_program_analysis() {
    let ws = Workspace::new();
    let (before, _) = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();
    ws.write("core", "fn core() -> String { \"broken\" }\n");
    let err = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap_err();
    assert!(err.starts_with("E0201:"), "unexpected diagnostic: {err}");

    ws.write("core", "fn core() -> Int { 99 }\n");
    let (repaired, observation) = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();
    assert!(matches!(
        invalidated_modules(&before, &repaired),
        Invalidation::Dirty { .. }
    ));
    assert_eq!(
        compile_resolved(&ws.file("main"), b"compiler-v1")
            .unwrap()
            .1,
        observation
    );
}

#[test]
fn missing_import_fails_resolution_without_producing_cacheable_snapshot() {
    let ws = Workspace::new();
    compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();
    fs::remove_file(ws.file("core")).unwrap();
    let err = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap_err();
    assert!(
        err.contains("cannot read") || err.contains("cannot resolve imported module"),
        "{err}"
    );
}

#[test]
fn transitive_effect_contract_change_is_checked_through_canonical_imports() {
    let ws = Workspace::new();
    ws.write("core", "fn core() -> Unit ! {} { () }\n");
    ws.write("api", "import core\nfn api() -> Unit ! {} { core() }\n");
    ws.write("main", "import api\nfn entry() -> Unit ! {} { api() }\n");
    let (before, initial) = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();
    assert!(!initial.bytecode.is_empty());

    ws.write(
        "core",
        "fn core() -> Unit ! {IO} { perform IO.print(\"effect\") }\n",
    );
    let err = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap_err();
    assert!(err.starts_with("E0301:"), "unexpected diagnostic: {err}");

    ws.write("api", "import core\nfn api() -> Unit ! {IO} { core() }\n");
    ws.write("main", "import api\nfn entry() -> Unit ! {IO} { api() }\n");
    let (after, changed) = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();
    let changed_modules = match invalidated_modules(&before, &after) {
        Invalidation::Dirty { modules } => modules,
        other => panic!("expected invalidation after transitive effect change: {other:?}"),
    };
    assert_eq!(changed_modules.len(), 3);
    assert_ne!(initial, changed);
    assert_eq!(
        compile_resolved(&ws.file("main"), b"compiler-v1")
            .unwrap()
            .1,
        changed
    );
}

#[test]
fn changed_compilation_context_forces_rebuild_on_resolved_graph() {
    let ws = Workspace::new();
    let (before, _) = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();
    let (after, _) = compile_resolved(&ws.file("main"), b"compiler-v2").unwrap();
    assert!(matches!(
        invalidated_modules(&before, &after),
        Invalidation::FullRebuild { .. }
    ));
}

#[test]
fn refuses_snapshot_when_root_changed_after_parsing() {
    let ws = Workspace::new();
    let root = ws.file("main");
    let old_source = fs::read_to_string(&root).unwrap();
    let tokens = Lexer::new(&old_source).lex().unwrap();
    let mut parsed = Parser::new(tokens).parse_module().unwrap();
    // Same import graph, different implementation: stale AST must never be
    // associated with a digest of source bytes that it did not parse.
    ws.write("main", "import api\nfn entry() -> Int { 100 }\n");
    let mut stack = HashSet::new();
    let err = resolve_imports_with_snapshot(
        &mut parsed,
        &root,
        &mut stack,
        old_source.as_bytes(),
        b"compiler-v1",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("changed since parsing"),
        "unexpected mismatch error: {err}"
    );
}

#[test]
fn canonical_snapshot_digests_match_exact_parsed_bytes() {
    let ws = Workspace::new();
    let (snapshot, _) = compile_resolved(&ws.file("main"), b"compiler-v1").unwrap();
    for name in ["core", "api", "main"] {
        let path = ws.file(name).canonicalize().unwrap();
        let key = path.to_string_lossy().into_owned();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            snapshot.modules[&key].source_digest,
            *blake3::hash(&bytes).as_bytes(),
            "wrong observed source for {name}"
        );
    }
}
