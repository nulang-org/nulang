//! Behavioral oracle for prospective incremental compiler reuse.
//! These tests do not enable incremental execution or authorize cache entries.

use nulang::package::incremental_oracle::{
    audit_incremental_plan, BuildObservation, CompilerOutput, OracleVerdict,
};
use nulang::package::module_invalidation::ModuleSnapshot;

fn fixture() -> ModuleSnapshot {
    let mut s = ModuleSnapshot::new(b"compiler=1,target=bytecode,flags=default");
    s.insert("core", b"fn core() { 1 }", [] as [&str; 0]);
    s.insert("api", b"fn api() { core() }", ["core"]);
    s.insert("unrelated", b"fn unrelated() { 5 }", [] as [&str; 0]);
    s
}

fn fresh_observation(snapshot: &ModuleSnapshot) -> BuildObservation {
    let mut result = BuildObservation::new(snapshot);
    for name in snapshot.modules.keys() {
        result.insert(
            name.clone(),
            CompilerOutput::from_bytes(
                b"no diagnostics",
                Some(format!("types/effects:{name}").as_bytes()),
                Some(format!("mir:{name}").as_bytes()),
                Some(format!("bytecode:{name}").as_bytes()),
            ),
        );
    }
    result
}

#[test]
fn accepts_matching_clean_and_candidate_outputs_for_a_conservative_frontier() {
    let before = fixture();
    let mut after = before.clone();
    after.insert("core", b"fn core() { 2 }", [] as [&str; 0]);
    let baseline = fresh_observation(&before);
    let mut clean = baseline.clone();
    clean.insert(
        "core",
        CompilerOutput::from_bytes(
            b"no diagnostics",
            Some(b"core typed 2"),
            Some(b"core mir 2"),
            Some(b"core bytecode 2"),
        ),
    );
    clean.insert(
        "api",
        CompilerOutput::from_bytes(
            b"no diagnostics",
            Some(b"api typed 2"),
            Some(b"api mir 2"),
            Some(b"api bytecode 2"),
        ),
    );
    assert_eq!(
        audit_incremental_plan(&before, &after, &baseline, &clean, Some(&clean)),
        OracleVerdict::Consistent {
            invalidated: vec!["api".into(), "core".into()],
            reusable: vec!["unrelated".into()],
            potentially_redundant: vec![],
            candidate_checked: true,
        }
    );
}

#[test]
fn rejects_an_unmarked_module_whose_fresh_semantics_changed() {
    let before = fixture();
    let mut after = before.clone();
    after.insert("core", b"fn core() { 2 }", [] as [&str; 0]);
    let baseline = fresh_observation(&before);
    let mut clean = baseline.clone();
    clean.insert(
        "unrelated",
        CompilerOutput::from_bytes(
            b"no diagnostics",
            Some(b"unexpected type/effect change"),
            Some(b"mir:unrelated"),
            Some(b"bytecode:unrelated"),
        ),
    );
    let result = audit_incremental_plan(&before, &after, &baseline, &clean, None);
    assert!(
        matches!(result, OracleVerdict::Rejected { reason } if reason.contains("unrelated") && reason.contains("typed_facts"))
    );
}

#[test]
fn rejects_a_candidate_using_stale_code_for_a_dirty_module() {
    let before = fixture();
    let mut after = before.clone();
    after.insert("core", b"fn core() { 2 }", [] as [&str; 0]);
    let baseline = fresh_observation(&before);
    let mut clean = baseline.clone();
    clean.insert(
        "core",
        CompilerOutput::from_bytes(
            b"no diagnostics",
            Some(b"changed"),
            Some(b"changed"),
            Some(b"changed"),
        ),
    );
    let result = audit_incremental_plan(&before, &after, &baseline, &clean, Some(&baseline));
    assert!(
        matches!(result, OracleVerdict::Rejected { reason } if reason.contains("candidate") && reason.contains("core") && reason.contains("artifact"))
    );
}

#[test]
fn rejects_incomplete_observation_sets_instead_of_silently_accepting() {
    let before = fixture();
    let after = before.clone();
    let baseline = fresh_observation(&before);
    let mut incomplete = baseline.clone();
    incomplete.modules.remove("api");
    let result = audit_incremental_plan(&before, &after, &baseline, &incomplete, None);
    assert!(
        matches!(result, OracleVerdict::Rejected { reason } if reason.contains("clean-after") && reason.contains("api"))
    );
}

#[test]
fn rejects_observations_from_the_wrong_compiler_context() {
    let before = fixture();
    let after = before.clone();
    let baseline = fresh_observation(&before);
    let mut wrong = baseline.clone();
    wrong.context_digest = [42; 32];
    let result = audit_incremental_plan(&before, &after, &baseline, &wrong, None);
    assert!(
        matches!(result, OracleVerdict::Rejected { reason } if reason.contains("context") && reason.contains("clean-after"))
    );
}

#[test]
fn marks_equal_outputs_as_possible_over_invalidation_not_as_proven_reuse() {
    let before = fixture();
    let mut after = before.clone();
    after.insert("core", b"// comment\nfn core() { 1 }", [] as [&str; 0]);
    let baseline = fresh_observation(&before);
    let result = audit_incremental_plan(&before, &after, &baseline, &baseline, None);
    assert_eq!(
        result,
        OracleVerdict::Consistent {
            invalidated: vec!["api".into(), "core".into()],
            reusable: vec!["unrelated".into()],
            potentially_redundant: vec!["api".into(), "core".into()],
            candidate_checked: false,
        }
    );
}

#[test]
fn a_changed_compilation_context_cannot_be_audited_as_reuse() {
    let before = fixture();
    let mut after = before.clone();
    after.context_digest = [99; 32];
    let baseline = fresh_observation(&before);
    let result = audit_incremental_plan(&before, &after, &baseline, &baseline, None);
    assert!(
        matches!(result, OracleVerdict::FullRebuildRequired { reason } if reason.contains("context"))
    );
}

fn compiled_output(source: &str) -> CompilerOutput {
    use nulang::{
        hir_lower, lexer::Lexer, mir_codegen, mir_lower, parser::Parser, typechecker::TypeChecker,
    };
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).expect("type check");
    let typed_facts = format!("{:?}", tc.inferred_decl_types);
    let hir = hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).expect("MIR lowering");
    let mir_text = format!("{mir:?}");
    let bytecode = mir_codegen::compile_mir(&mut mir, "oracle").expect("bytecode");
    let bytes = bytecode.to_nbc(None).expect("serialize bytecode");
    CompilerOutput::from_bytes(
        b"",
        Some(typed_facts.as_bytes()),
        Some(mir_text.as_bytes()),
        Some(&bytes),
    )
}

#[test]
fn a_real_bytecode_compile_is_detected_as_stale_when_source_changes() {
    let original = b"1 + 2";
    let edited = b"1 + 3";
    let mut before = ModuleSnapshot::new(b"compiler=1,bytecode");
    before.insert("main", original, [] as [&str; 0]);
    let mut after = before.clone();
    after.insert("main", edited, [] as [&str; 0]);

    let mut baseline = BuildObservation::new(&before);
    baseline.insert(
        "main",
        compiled_output(std::str::from_utf8(original).unwrap()),
    );
    let mut clean = BuildObservation::new(&after);
    clean.insert(
        "main",
        compiled_output(std::str::from_utf8(edited).unwrap()),
    );

    assert!(matches!(
        audit_incremental_plan(&before, &after, &baseline, &clean, Some(&baseline)),
        OracleVerdict::Rejected { reason } if reason.contains("candidate") && reason.contains("main") && reason.contains("artifact")
    ));
    assert!(matches!(
        audit_incremental_plan(&before, &after, &baseline, &clean, Some(&clean)),
        OracleVerdict::Consistent {
            candidate_checked: true,
            ..
        }
    ));
}
