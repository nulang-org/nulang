//! Canonical-resolver input-graph regression tests. No compilation reuse.
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::package::import_observation::observe_resolved_imports;
use nulang::package::module_invalidation::{invalidated_modules, Invalidation};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("nulang-resolver-input-{}-{id}", std::process::id()));
        fs::create_dir(&root).expect("new isolated fixture directory");
        Self { root }
    }
    fn write(&self, name: &str, source: &str) -> PathBuf {
        let p = self.root.join(name);
        fs::write(&p, source).expect("write fixture");
        p
    }
    fn observe(
        &self,
        root_name: &str,
    ) -> nulang::package::import_observation::ResolvedImportObservation {
        let path = self.root.join(root_name);
        let source = fs::read(&path).expect("read root");
        observe_resolved_imports(&path, &source, b"compiler=v1;target=bytecode")
            .expect("observed resolution succeeds")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn key(p: &Path) -> String {
    p.canonicalize()
        .unwrap()
        .to_str()
        .expect("UTF-8 fixture path")
        .to_string()
}

#[test]
fn captures_exact_transitive_import_edges_and_source_digests() {
    let t = Fixture::new();
    let root = t.write("root.nula", "import middle\nfn root() { middle() }");
    let mid = t.write("middle.nula", "import leaf\nfn middle() { leaf() }");
    let leaf = t.write("leaf.nula", "fn leaf() { 1 }");
    let out = t.observe("root.nula");
    let graph = &out.snapshot.modules;
    assert_eq!(graph.len(), 3);
    assert_eq!(graph[&key(&root)].imports, BTreeSet::from([key(&mid)]));
    assert_eq!(graph[&key(&mid)].imports, BTreeSet::from([key(&leaf)]));
    assert!(graph[&key(&leaf)].imports.is_empty());
    assert_eq!(
        graph[&key(&leaf)].source_digest,
        *blake3::hash(b"fn leaf() { 1 }").as_bytes()
    );
    assert!(out
        .module
        .decls
        .iter()
        .all(|d| !matches!(d, nulang::ast::Decl::Import { .. })));
}

#[test]
fn leaf_edit_reaches_root_via_the_real_resolved_dependency_graph() {
    let t = Fixture::new();
    let root = t.write("root.nula", "import middle\nfn root() { middle() }");
    let middle = t.write("middle.nula", "import leaf\nfn middle() { leaf() }");
    let leaf = t.write("leaf.nula", "fn leaf() { 1 }");
    let before = t.observe("root.nula");
    t.write("leaf.nula", "fn leaf() { 2 }");
    let after = t.observe("root.nula");
    assert_eq!(
        invalidated_modules(&before.snapshot, &after.snapshot),
        Invalidation::Dirty {
            modules: vec![key(&leaf), key(&middle), key(&root)]
        }
    );
}

#[test]
fn repeated_cached_import_retains_its_transitive_edges() {
    let t = Fixture::new();
    let root = t.write(
        "root.nula",
        "import middle\nimport middle\nfn root() { middle() }",
    );
    let middle = t.write("middle.nula", "import shared\nfn middle() { shared() }");
    let shared = t.write("shared.nula", "fn shared() { 10 }");
    let out = t.observe("root.nula");
    assert_eq!(out.snapshot.modules.len(), 3);
    assert_eq!(
        out.snapshot.modules[&key(&root)].imports,
        BTreeSet::from([key(&middle)])
    );
    assert_eq!(
        out.snapshot.modules[&key(&middle)].imports,
        BTreeSet::from([key(&shared)])
    );
}

#[test]
fn missing_dependency_or_cycle_is_rejected_by_the_canonical_resolver() {
    let t = Fixture::new();
    let root = t.write("root.nula", "import missing\nfn root() { 1 }");
    assert!(observe_resolved_imports(&root, &fs::read(&root).unwrap(), b"context").is_err());
    t.write("root.nula", "import child\nfn root() { 1 }");
    t.write("child.nula", "import root\nfn child() { 1 }");
    assert!(observe_resolved_imports(&root, &fs::read(&root).unwrap(), b"context").is_err());
}

#[test]
fn changing_only_resolution_context_requires_conservative_rebuild() {
    let t = Fixture::new();
    let root = t.write("root.nula", "fn root() { 1 }");
    let src = fs::read(&root).unwrap();
    let before = observe_resolved_imports(&root, &src, b"target=x86").unwrap();
    let after = observe_resolved_imports(&root, &src, b"target=aarch64").unwrap();
    assert!(
        matches!(invalidated_modules(&before.snapshot,&after.snapshot),
        Invalidation::FullRebuild{reason} if reason.contains("context"))
    );
}

#[test]
fn modified_in_memory_root_source_is_the_observed_input_not_stale_disk_bytes() {
    let t = Fixture::new();
    let root = t.write("root.nula", "fn root() { 1 }");
    let before = t.observe("root.nula");
    let modified = b"fn root() { 99 }";
    let after = observe_resolved_imports(&root, modified, b"compiler=v1;target=bytecode").unwrap();
    assert_eq!(
        after.snapshot.modules[&key(&root)].source_digest,
        *blake3::hash(modified).as_bytes()
    );
    assert_eq!(
        invalidated_modules(&before.snapshot, &after.snapshot),
        Invalidation::Dirty {
            modules: vec![key(&root)]
        }
    );
}

#[test]
fn missing_root_file_and_invalid_utf8_are_rejected() {
    let t = Fixture::new();
    let root = t.root.join("missing.nula");
    assert!(observe_resolved_imports(&root, b"fn root() { 1 }", b"context").is_err());
    let root = t.write("root.nula", "fn root() { 1 }");
    assert!(observe_resolved_imports(&root, b"\xff", b"context").is_err());
}
