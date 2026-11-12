//! Compiler-owned observations for the fail-closed incremental build oracle.
//! A clean observation compiles each standalone source through actual frontend
//! and bytecode compiler components; it never authorizes cache reuse.
use nulang::package::{
    clean_observation::observe_unlinked_bytecode_sources,
    incremental_oracle::{audit_incremental_plan, OracleVerdict},
};

fn observe<'a>(
    sources: &[(&'a str, &'a str)],
) -> nulang::package::clean_observation::CleanObservations {
    observe_unlinked_bytecode_sources(sources.iter().map(|(name, src)| (*name, src.as_bytes())))
        .expect("standalone compilation must succeed")
}

#[test]
fn independently_compiled_same_source_produces_identical_stage_digests() {
    let a = observe(&[("core", "fn value() -> Int { 1 + 2 }")]);
    let b = observe(&[("core", "fn value() -> Int { 1 + 2 }")]);
    assert_eq!(a.snapshot, b.snapshot);
    assert_eq!(a.build, b.build);
    let out = &a.build.modules["core"];
    assert!(out.typed_facts.is_some());
    assert!(out.mir.is_some());
    assert!(out.artifact.is_some());
}

#[test]
fn detects_real_stale_bytecode_after_body_edit() {
    let old = observe(&[
        ("core", "fn value() -> Int { 1 + 2 }"),
        ("other", "fn other() -> Int { 100 }"),
    ]);
    let new = observe(&[
        ("core", "fn value() -> Int { 1 + 3 }"),
        ("other", "fn other() -> Int { 100 }"),
    ]);
    assert!(matches!(
        audit_incremental_plan(&old.snapshot, &new.snapshot, &old.build, &new.build, Some(&old.build)),
        OracleVerdict::Rejected { reason } if reason.contains("candidate") && reason.contains("core") && reason.contains("artifact")
    ));
    assert!(matches!(
        audit_incremental_plan(&old.snapshot, &new.snapshot, &old.build, &new.build, Some(&new.build)),
        OracleVerdict::Consistent { invalidated, reusable, candidate_checked: true, .. }
            if invalidated == ["core"] && reusable == ["other"]
    ));
}

#[test]
fn signature_edit_changes_compiler_owned_observation() {
    let old = observe(&[("core", "fn value(x: Int) -> Int { x + 1 }")]);
    let new = observe(&[("core", "fn value(x: Float) -> Float { x + 1.0 }")]);
    assert_ne!(
        old.build.modules["core"].typed_facts, new.build.modules["core"].typed_facts,
        "typed compiler facts must reflect the signature change"
    );
    assert!(matches!(
        audit_incremental_plan(
            &old.snapshot,
            &new.snapshot,
            &old.build,
            &new.build,
            Some(&old.build)
        ),
        OracleVerdict::Rejected { .. }
    ));
}

#[test]
fn rejects_imports_until_the_compiler_owned_dependency_graph_is_integrated() {
    let error = observe_unlinked_bytecode_sources([(
        "main",
        b"import \"missing.nula\"\nfn value() -> Int { 1 }".as_slice(),
    )])
    .expect_err("imports must fail closed");
    assert!(error.contains("import"), "{error}");
}

#[test]
fn rejects_effectful_source_in_pure_observation_mode() {
    let error = observe_unlinked_bytecode_sources([(
        "core",
        b"fn report() { perform IO.print(\"hello\") }".as_slice(),
    )])
    .expect_err("effectful input is outside the standalone pure-profile corpus");
    assert!(
        error.contains("effect") || error.contains("unsupported"),
        "{error}"
    );
}

#[test]
fn rejects_malformed_source_and_duplicate_module_names() {
    let malformed =
        observe_unlinked_bytecode_sources([("core", b"fn value() -> Int {".as_slice())]);
    assert!(malformed.is_err());

    let duplicate = observe_unlinked_bytecode_sources([
        ("core", b"fn value() -> Int { 1 }".as_slice()),
        ("core", b"fn value() -> Int { 2 }".as_slice()),
    ]);
    assert!(duplicate
        .expect_err("duplicate name must fail")
        .contains("duplicate"));
}

#[test]
fn rejects_non_utf8_and_empty_module_set() {
    assert!(observe_unlinked_bytecode_sources([("core", &[0xff][..])]).is_err());
    assert!(observe_unlinked_bytecode_sources(std::iter::empty::<(&str, &[u8])>()).is_err());
}
