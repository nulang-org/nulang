//! Diagnostic-only differential evidence for module invalidation.
//!
//! This uses real Nulang parsing, type/effect checking, and bytecode compilation,
//! but deliberately compiles each fixture module independently. Import edges are
//! supplied by the fixture, NOT collected from the canonical module resolver.
//! These tests neither enable incremental compilation nor prove safe cache reuse,
//! linked-module semantics, or cross-backend artifact identity.

use std::collections::{BTreeMap, BTreeSet};

use nulang::effect_checker::EffectChecker;
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::package::module_invalidation::{invalidated_modules, Invalidation, ModuleSnapshot};
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::types::NuError;

#[derive(Clone)]
struct SourceFile {
    text: &'static str,
    imports: Vec<&'static str>,
}

type Workspace = BTreeMap<&'static str, SourceFile>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Observation {
    Accepted {
        inferred_types: Vec<String>,
        bytecode: Vec<String>,
        constants: Vec<String>,
        effect_sites: Vec<String>,
    },
    Rejected(String),
}

fn diagnostic(error: NuError) -> String {
    format!("{}: {error}", error.stable_code().unwrap_or("unknown"))
}

fn observe(source: &str) -> Observation {
    let compilation = || -> Result<Observation, String> {
        let tokens = Lexer::new(source).lex().map_err(diagnostic)?;
        let ast = Parser::new(tokens).parse_module().map_err(diagnostic)?;
        let mut checker = TypeChecker::new();
        checker.check_module(&ast).map_err(diagnostic)?;
        EffectChecker::new()
            .check_module(&ast.decls)
            .map_err(diagnostic)?;

        let mut inferred_types: Vec<_> = checker
            .inferred_decl_types
            .iter()
            .map(|(name, typ)| format!("{name}: {typ:?}"))
            .collect();
        inferred_types.sort();

        let hir = hir_lower::lower_module(&ast, &checker.inferred_decl_types);
        let mut mir = mir_lower::lower_module(&hir).map_err(diagnostic)?;
        let compiled = mir_codegen::compile_mir(&mut mir, "parity-fixture").map_err(diagnostic)?;
        Ok(Observation::Accepted {
            inferred_types,
            bytecode: compiled
                .instructions
                .iter()
                .map(|instruction| format!("{:?}", instruction.encode()))
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
        })
    };
    compilation().unwrap_or_else(Observation::Rejected)
}

fn workspace() -> Workspace {
    BTreeMap::from([
        (
            "core",
            SourceFile {
                text: "fn core() -> Int { 1 }",
                imports: vec![],
            },
        ),
        (
            "api",
            SourceFile {
                text: "fn api() -> Int { 4 }",
                imports: vec!["core"],
            },
        ),
        (
            "app",
            SourceFile {
                text: "fn entry() -> Int { 5 }",
                imports: vec!["api"],
            },
        ),
        (
            "unrelated",
            SourceFile {
                text: "fn unrelated() -> Bool { true }",
                imports: vec![],
            },
        ),
    ])
}

fn snapshot(workspace: &Workspace, context: &[u8]) -> ModuleSnapshot {
    let mut snapshot = ModuleSnapshot::new(context);
    for (name, file) in workspace {
        snapshot.insert(*name, file.text.as_bytes(), file.imports.iter().copied());
    }
    snapshot
}

fn clean_observations(workspace: &Workspace) -> BTreeMap<String, Observation> {
    workspace
        .iter()
        .map(|(name, file)| (name.to_string(), observe(file.text)))
        .collect()
}

fn changed_observations(
    before: &ModuleSnapshot,
    after: &ModuleSnapshot,
    workspace: &Workspace,
    cached: &BTreeMap<String, Observation>,
) -> (Invalidation, BTreeMap<String, Observation>) {
    let invalidation = invalidated_modules(before, after);
    let mut observations = cached.clone();
    match &invalidation {
        Invalidation::Unchanged => {}
        Invalidation::Dirty { modules } => {
            for name in modules {
                let file = workspace.get(name.as_str()).expect("dirty module present");
                observations.insert(name.clone(), observe(file.text));
            }
        }
        Invalidation::FullRebuild { .. } => {
            observations = clean_observations(workspace);
        }
    }
    (invalidation, observations)
}

#[test]
fn body_change_rechecks_reverse_dependents_and_matches_clean_observations() {
    let before = workspace();
    let initial = snapshot(&before, b"test-compiler-context");
    let cached = clean_observations(&before);
    assert!(cached
        .values()
        .all(|value| matches!(value, Observation::Accepted { .. })));

    let mut after = before.clone();
    after.get_mut("core").unwrap().text = "fn core() -> Int { 42 }";
    let updated = snapshot(&after, b"test-compiler-context");
    let (plan, incremental) = changed_observations(&initial, &updated, &after, &cached);
    assert_eq!(
        plan,
        Invalidation::Dirty {
            modules: vec!["api".into(), "app".into(), "core".into()],
        }
    );
    assert_eq!(incremental, clean_observations(&after));
    assert_ne!(incremental["core"], cached["core"]);
    assert_eq!(incremental["unrelated"], cached["unrelated"]);
}

#[test]
fn type_error_and_repair_have_exact_clean_diagnostic_parity() {
    let initial_workspace = workspace();
    let initial = snapshot(&initial_workspace, b"test-compiler-context");
    let cached = clean_observations(&initial_workspace);

    let mut broken = initial_workspace.clone();
    broken.get_mut("core").unwrap().text = "fn core() -> Int { \"invalid\" }";
    let broken_snapshot = snapshot(&broken, b"test-compiler-context");
    let (_, broken_observations) =
        changed_observations(&initial, &broken_snapshot, &broken, &cached);
    assert_eq!(broken_observations, clean_observations(&broken));
    assert!(
        matches!(&broken_observations["core"], Observation::Rejected(reason) if reason.starts_with("E0201:"))
    );

    let mut repaired = broken.clone();
    repaired.get_mut("core").unwrap().text = "fn core() -> Int { 99 }";
    let repaired_snapshot = snapshot(&repaired, b"test-compiler-context");
    let (plan, repaired_observations) = changed_observations(
        &broken_snapshot,
        &repaired_snapshot,
        &repaired,
        &broken_observations,
    );
    assert!(matches!(plan, Invalidation::Dirty { .. }));
    assert_eq!(repaired_observations, clean_observations(&repaired));
    assert!(matches!(
        &repaired_observations["core"],
        Observation::Accepted { .. }
    ));
}

#[test]
fn effect_row_violation_matches_clean_compiler_diagnostics() {
    let mut before = workspace();
    before.get_mut("core").unwrap().text = "fn core() -> Unit ! {IO} { perform IO.print(\"ok\") }";
    let initial = snapshot(&before, b"test-compiler-context");
    let cached = clean_observations(&before);
    assert!(matches!(&cached["core"], Observation::Accepted { .. }));

    let mut after = before.clone();
    after.get_mut("core").unwrap().text = "fn core() -> Unit ! {} { perform IO.print(\"ok\") }";
    let updated = snapshot(&after, b"test-compiler-context");
    let (plan, incremental) = changed_observations(&initial, &updated, &after, &cached);
    assert!(matches!(plan, Invalidation::Dirty { .. }));
    assert_eq!(incremental, clean_observations(&after));
    assert!(
        matches!(&incremental["core"], Observation::Rejected(reason) if reason.starts_with("E0301:"))
    );
}

#[test]
fn invalid_graph_or_compiler_context_falls_back_to_clean_observations() {
    let before = workspace();
    let initial = snapshot(&before, b"test-compiler-context");
    let cached = clean_observations(&before);

    let changed_context = snapshot(&before, b"another-backend-or-compiler-context");
    let (plan, output) = changed_observations(&initial, &changed_context, &before, &cached);
    assert!(matches!(plan, Invalidation::FullRebuild { .. }));
    assert_eq!(output, clean_observations(&before));

    let mut missing_import = before.clone();
    missing_import
        .get_mut("api")
        .unwrap()
        .imports
        .push("missing");
    let invalid = snapshot(&missing_import, b"test-compiler-context");
    let (plan, output) = changed_observations(&initial, &invalid, &missing_import, &cached);
    assert!(matches!(plan, Invalidation::FullRebuild { .. }));
    assert_eq!(output, clean_observations(&missing_import));
}

#[test]
fn import_edge_edit_uses_historical_and_current_dependents() {
    let before = workspace();
    let initial = snapshot(&before, b"test-compiler-context");
    let cached = clean_observations(&before);

    let mut after = before.clone();
    after.get_mut("api").unwrap().imports = vec!["unrelated"];
    let updated = snapshot(&after, b"test-compiler-context");
    let (plan, observations) = changed_observations(&initial, &updated, &after, &cached);

    assert_eq!(
        plan,
        Invalidation::Dirty {
            modules: vec!["api".into(), "app".into()],
        }
    );
    assert_eq!(observations, clean_observations(&after));
    assert_eq!(
        updated.modules["api"].imports,
        BTreeSet::from(["unrelated".to_string()])
    );
}
