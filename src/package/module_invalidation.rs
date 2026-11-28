//! Conservative, module-level invalidation planning for future incremental
//! compilation. This module never authorizes reuse of compiled code.
//!
//! Compiler integration must supply the fully resolved module graph, exact
//! module source bytes, and a complete compiler-input context. When any of
//! those facts are incomplete, callers must perform a clean compilation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Exact input bytes for a module, plus compiler-resolved module dependencies.
/// Not a SemanticId: equal source bytes alone cannot establish equal semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInput {
    pub source_digest: [u8; 32],
    pub imports: BTreeSet<String>,
}

impl ModuleInput {
    pub fn from_source<I, S>(source: &[u8], imports: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            source_digest: *blake3::hash(source).as_bytes(),
            imports: imports.into_iter().map(Into::into).collect(),
        }
    }
}

/// One observed compiler-input snapshot. `context_digest` is an opaque
/// caller-provided fingerprint of target, backend, flags, compiler/toolchain,
/// ABI, package configuration, and every other non-module input.
///
/// The compiler has not yet proved this input closure complete. Consequently,
/// a matching snapshot is only an invalidation diagnostic, not a cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSnapshot {
    pub context_digest: [u8; 32],
    pub modules: BTreeMap<String, ModuleInput>,
}

impl ModuleSnapshot {
    pub fn new(context_bytes: &[u8]) -> Self {
        Self {
            context_digest: *blake3::hash(context_bytes).as_bytes(),
            modules: BTreeMap::new(),
        }
    }

    pub fn insert<I, S>(&mut self, name: impl Into<String>, source: &[u8], imports: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.modules
            .insert(name.into(), ModuleInput::from_source(source, imports));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalidation {
    /// No observed module or context changes. This is NOT proof of cache safety.
    Unchanged,
    /// Modules that need fresh checking/lowering, including reverse dependents.
    Dirty { modules: Vec<String> },
    /// Missing/ambiguous graph inputs require a clean compiler invocation.
    FullRebuild { reason: String },
}

/// Diagnostic explanation of observed changes. Neither the report nor its
/// dependency order authorizes skipping checks or reusing artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidationReport {
    pub invalidation: Invalidation,
    /// Direct source/import changes, in lexical order.
    pub changed_modules: Vec<String>,
    /// Affected modules in dependency-first order in the current graph.
    /// Empty when unchanged or when a clean compilation is required.
    pub affected_in_dependency_order: Vec<String>,
}

impl InvalidationReport {
    fn full_rebuild(reason: String) -> Self {
        Self {
            invalidation: Invalidation::FullRebuild { reason },
            changed_modules: Vec::new(),
            affected_in_dependency_order: Vec::new(),
        }
    }
}

/// Compute a conservative invalidation frontier using source changes and both
/// historical and current dependency edges. The caller must still execute
/// normal type/effect/capability checking and compare fresh compiler outputs.
pub fn invalidated_modules(before: &ModuleSnapshot, after: &ModuleSnapshot) -> Invalidation {
    explain_invalidation(before, after).invalidation
}

/// Explain direct changes and their transitive impact without changing the
/// existing conservative invalidation policy.
pub fn explain_invalidation(before: &ModuleSnapshot, after: &ModuleSnapshot) -> InvalidationReport {
    if before.context_digest != after.context_digest {
        return InvalidationReport::full_rebuild("compilation context changed".into());
    }
    if let Err(reason) = validate_graph(before) {
        return InvalidationReport::full_rebuild(format!(
            "before module graph is incomplete or invalid: {reason}"
        ));
    }
    let dependency_order = match validate_graph(after) {
        Ok(order) => order,
        Err(reason) => {
            return InvalidationReport::full_rebuild(format!(
                "after module graph is incomplete or invalid: {reason}"
            ));
        }
    };

    // Deletions may invalidate source-level references beyond explicit import
    // edges, especially until symbol-resolution dependencies are complete.
    if before
        .modules
        .keys()
        .any(|name| !after.modules.contains_key(name))
    {
        return InvalidationReport::full_rebuild("module removed".into());
    }

    let mut dirty = BTreeSet::new();
    for (name, module) in &after.modules {
        if before.modules.get(name) != Some(module) {
            dirty.insert(name.clone());
        }
    }
    let changed_modules = dirty.iter().cloned().collect();
    if dirty.is_empty() {
        return InvalidationReport {
            invalidation: Invalidation::Unchanged,
            changed_modules,
            affected_in_dependency_order: Vec::new(),
        };
    }

    // Use both graphs: an import removed from a module is still a change that
    // must invalidate the old dependents, not just the new ones.
    let mut reverse: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for snapshot in [before, after] {
        for (module, input) in &snapshot.modules {
            for dependency in &input.imports {
                reverse
                    .entry(dependency.clone())
                    .or_default()
                    .insert(module.clone());
            }
        }
    }

    let mut queue: VecDeque<String> = dirty.iter().cloned().collect();
    while let Some(changed) = queue.pop_front() {
        if let Some(dependents) = reverse.get(&changed) {
            for dependent in dependents {
                if dirty.insert(dependent.clone()) {
                    queue.push_back(dependent.clone());
                }
            }
        }
    }
    let affected_in_dependency_order = dependency_order
        .into_iter()
        .filter(|name| dirty.contains(name))
        .collect();
    InvalidationReport {
        invalidation: Invalidation::Dirty {
            modules: dirty.into_iter().collect(),
        },
        changed_modules,
        affected_in_dependency_order,
    }
}

fn validate_graph(snapshot: &ModuleSnapshot) -> Result<Vec<String>, String> {
    // Kahn's algorithm avoids unbounded recursive DFS on untrusted module
    // graphs and deterministically rejects cycles or unresolved imports.
    let mut indegrees: BTreeMap<String, usize> = snapshot
        .modules
        .keys()
        .map(|name| (name.clone(), 0))
        .collect();
    let mut reverse: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for (name, module) in &snapshot.modules {
        if name.is_empty() {
            return Err("empty module name".into());
        }
        for dependency in &module.imports {
            if !snapshot.modules.contains_key(dependency) {
                return Err(format!(
                    "unresolved module import '{dependency}' in '{name}'"
                ));
            }
            *indegrees.get_mut(name).expect("registered module") += 1;
            reverse
                .entry(dependency.clone())
                .or_default()
                .insert(name.clone());
        }
    }

    let mut ready: BTreeSet<String> = indegrees
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(name, _)| name.clone())
        .collect();
    let mut order = Vec::with_capacity(snapshot.modules.len());
    while let Some(name) = ready.pop_first() {
        if let Some(dependents) = reverse.get(&name) {
            for dependent in dependents {
                let degree = indegrees.get_mut(dependent).expect("validated module");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(dependent.clone());
                }
            }
        }
        order.push(name);
    }
    if order.len() != snapshot.modules.len() {
        return Err("module dependency cycle".into());
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(context: &[u8]) -> ModuleSnapshot {
        let mut snapshot = ModuleSnapshot::new(context);
        snapshot.insert("core", b"fn core() { 1 }", [] as [&str; 0]);
        snapshot.insert("api", b"fn api() { core() }", ["core"]);
        snapshot.insert("app", b"fn app() { api() }", ["api"]);
        snapshot.insert("unrelated", b"fn unrelated() { 3 }", [] as [&str; 0]);
        snapshot
    }

    #[test]
    fn impact_reports_roots_and_dependency_order() {
        let before = fixture(b"context");
        let mut after = before.clone();
        after.insert("core", b"new core", [] as [&str; 0]);
        let report = explain_invalidation(&before, &after);
        assert_eq!(report.changed_modules, vec!["core"]);
        assert_eq!(
            report.affected_in_dependency_order,
            vec!["core", "api", "app"]
        );
        assert_eq!(report.invalidation, invalidated_modules(&before, &after));
    }

    #[test]
    fn impact_explains_multiple_roots_with_deterministic_order() {
        let before = fixture(b"context");
        let mut after = before.clone();
        after.insert("core", b"new core", [] as [&str; 0]);
        after.insert("unrelated", b"new unrelated", [] as [&str; 0]);
        let report = explain_invalidation(&before, &after);
        assert_eq!(report.changed_modules, vec!["core", "unrelated"]);
        assert_eq!(
            report.affected_in_dependency_order,
            vec!["core", "api", "app", "unrelated"]
        );
    }

    #[test]
    fn invalid_graph_never_exposes_partial_execution_plan() {
        let before = fixture(b"context");
        let mut after = before.clone();
        after.insert("core", b"cycle", ["api"]);
        let report = explain_invalidation(&before, &after);
        assert!(matches!(
            report.invalidation,
            Invalidation::FullRebuild { .. }
        ));
        assert!(report.changed_modules.is_empty());
        assert!(report.affected_in_dependency_order.is_empty());
    }

    #[test]
    fn unchanged_graph_has_no_affected_order() {
        let before = fixture(b"context");
        let report = explain_invalidation(&before, &before);
        assert_eq!(report.invalidation, Invalidation::Unchanged);
        assert!(report.changed_modules.is_empty());
        assert!(report.affected_in_dependency_order.is_empty());
    }

    #[test]
    fn same_inputs_do_not_mark_any_module_dirty() {
        let initial = fixture(b"compiler=1,target=wasm");
        assert_eq!(
            invalidated_modules(&initial, &initial),
            Invalidation::Unchanged
        );
    }

    #[test]
    fn changed_module_invalidates_only_its_reverse_dependents() {
        let before = fixture(b"compiler=1,target=wasm");
        let mut after = before.clone();
        after.insert("core", b"fn core() { 2 }", [] as [&str; 0]);
        assert_eq!(
            invalidated_modules(&before, &after),
            Invalidation::Dirty {
                modules: vec!["api".into(), "app".into(), "core".into()]
            }
        );
    }

    #[test]
    fn independent_edit_does_not_invalidate_other_modules() {
        let before = fixture(b"compiler=1,target=wasm");
        let mut after = before.clone();
        after.insert("unrelated", b"fn unrelated() { 4 }", [] as [&str; 0]);
        assert_eq!(
            invalidated_modules(&before, &after),
            Invalidation::Dirty {
                modules: vec!["unrelated".into()]
            }
        );
    }

    #[test]
    fn import_edge_change_invalidates_module_and_its_dependents() {
        let before = fixture(b"compiler=1,target=wasm");
        let mut after = before.clone();
        after.insert("api", b"fn api() { core() }", ["core", "unrelated"]);
        assert_eq!(
            invalidated_modules(&before, &after),
            Invalidation::Dirty {
                modules: vec!["api".into(), "app".into()]
            }
        );
    }

    #[test]
    fn compiler_context_change_requires_a_full_rebuild() {
        let before = fixture(b"compiler=1,target=wasm");
        let after = fixture(b"compiler=2,target=wasm");
        assert!(matches!(
            invalidated_modules(&before, &after),
            Invalidation::FullRebuild { .. }
        ));
    }

    #[test]
    fn removed_module_requires_full_rebuild() {
        let before = fixture(b"compiler=1,target=wasm");
        let mut after = before.clone();
        after.modules.remove("unrelated");
        assert!(matches!(
            invalidated_modules(&before, &after),
            Invalidation::FullRebuild { .. }
        ));
    }

    #[test]
    fn missing_module_import_requires_full_rebuild() {
        let before = fixture(b"compiler=1,target=wasm");
        let mut after = before.clone();
        after.insert("api", b"fn api() { missing() }", ["missing"]);
        assert!(matches!(
            invalidated_modules(&before, &after),
            Invalidation::FullRebuild { .. }
        ));
    }

    #[test]
    fn cyclic_module_import_requires_full_rebuild() {
        let before = fixture(b"compiler=1,target=wasm");
        let mut after = before.clone();
        after.insert("core", b"fn core() { api() }", ["api"]);
        assert!(matches!(
            invalidated_modules(&before, &after),
            Invalidation::FullRebuild { .. }
        ));
    }

    #[test]
    fn adding_module_only_invalidates_that_module_until_it_is_imported() {
        let before = fixture(b"compiler=1,target=wasm");
        let mut after = before.clone();
        after.insert("new", b"fn new() { 4 }", [] as [&str; 0]);
        assert_eq!(
            invalidated_modules(&before, &after),
            Invalidation::Dirty {
                modules: vec!["new".into()]
            }
        );
    }
}
