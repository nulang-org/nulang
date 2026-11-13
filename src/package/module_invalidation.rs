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

/// Compiler-emitted, canonical public-interface observations for a module graph.
///
/// A fingerprint must cover every publicly observable contract, including
/// exported types, inferred effects, reference capabilities, actor protocols,
/// and any schema/ABI identity exposed by the module. The planner cannot
/// authenticate the supplied bytes or prove that this closure is complete.
/// Do not construct fingerprints from source-text heuristics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInterfaces {
    context_digest: [u8; 32],
    fingerprints: BTreeMap<String, [u8; 32]>,
}

impl ModuleInterfaces {
    pub fn new(snapshot: &ModuleSnapshot) -> Self {
        Self {
            context_digest: snapshot.context_digest,
            fingerprints: BTreeMap::new(),
        }
    }

    /// Hash the complete deterministic interface serialization emitted by the
    /// compiler. This API does not assert the bytes are canonical or complete.
    pub fn insert(&mut self, name: impl Into<String>, interface_bytes: &[u8]) {
        self.fingerprints
            .insert(name.into(), *blake3::hash(interface_bytes).as_bytes());
    }
}

/// A diagnostic-only estimate of which compilation stages a change may affect.
/// This is NOT permission to skip downstream type checking or reuse artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterfaceInvalidation {
    Unchanged,
    Dirty {
        /// Source, import, or interface observations changed for these modules.
        changed_modules: Vec<String>,
        /// Other modules transitively importing a changed public interface.
        affected_dependents: Vec<String>,
    },
    FullRebuild {
        reason: String,
    },
}

/// Compare compiler-produced public interfaces while retaining all existing
/// fail-closed checks of the conservative module invalidation planner.
///
/// Private implementation edits can be distinguished from interface changes
/// for diagnostics only. Real incremental reuse still requires compiler-owned
/// complete input closure, deterministic interface serialization, and clean
/// versus incremental semantic/output parity across every supported backend.
pub fn analyze_interface_invalidation(
    before: &ModuleSnapshot,
    after: &ModuleSnapshot,
    before_interfaces: &ModuleInterfaces,
    after_interfaces: &ModuleInterfaces,
) -> InterfaceInvalidation {
    if let Invalidation::FullRebuild { reason } = invalidated_modules(before, after) {
        return InterfaceInvalidation::FullRebuild { reason };
    }
    for (label, snapshot, observed) in [
        ("before", before, before_interfaces),
        ("after", after, after_interfaces),
    ] {
        if observed.context_digest != snapshot.context_digest {
            return InterfaceInvalidation::FullRebuild {
                reason: format!(
                    "{label} interface observations use a different compilation context"
                ),
            };
        }
        if !observed.fingerprints.keys().eq(snapshot.modules.keys()) {
            return InterfaceInvalidation::FullRebuild {
                reason: format!(
                    "{label} interface observations do not cover exactly the module graph"
                ),
            };
        }
    }

    let mut changed = BTreeSet::new();
    let mut changed_interfaces = BTreeSet::new();
    for (name, input) in &after.modules {
        let previous = before.modules.get(name);
        let interface_changed =
            before_interfaces.fingerprints.get(name) != after_interfaces.fingerprints.get(name);
        let imports_changed = previous.map(|old| &old.imports) != Some(&input.imports);
        if previous != Some(input) || interface_changed {
            changed.insert(name.clone());
        }
        // Changed imports can change public behavior even if a caller supplied
        // identical interface bytes, so be conservative until closure is proven.
        if imports_changed || interface_changed {
            changed_interfaces.insert(name.clone());
        }
    }

    if changed.is_empty() {
        return InterfaceInvalidation::Unchanged;
    }

    let mut reverse: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for snapshot in [before, after] {
        for (name, input) in &snapshot.modules {
            for dependency in &input.imports {
                reverse
                    .entry(dependency.clone())
                    .or_default()
                    .insert(name.clone());
            }
        }
    }

    let mut affected = BTreeSet::new();
    let mut queue: VecDeque<String> = changed_interfaces.into_iter().collect();
    while let Some(module) = queue.pop_front() {
        if let Some(dependents) = reverse.get(&module) {
            for dependent in dependents {
                if affected.insert(dependent.clone()) {
                    queue.push_back(dependent.clone());
                }
            }
        }
    }
    // Modules already listed as locally changed must not be reported twice.
    affected.retain(|name| !changed.contains(name));
    InterfaceInvalidation::Dirty {
        changed_modules: changed.into_iter().collect(),
        affected_dependents: affected.into_iter().collect(),
    }
}

/// Compute a conservative invalidation frontier using source changes and both
/// historical and current dependency edges. The caller must still execute
/// normal type/effect/capability checking and compare fresh compiler outputs.
pub fn invalidated_modules(before: &ModuleSnapshot, after: &ModuleSnapshot) -> Invalidation {
    if before.context_digest != after.context_digest {
        return Invalidation::FullRebuild {
            reason: "compilation context changed".into(),
        };
    }
    for (label, snapshot) in [("before", before), ("after", after)] {
        if let Err(reason) = validate_graph(snapshot) {
            return Invalidation::FullRebuild {
                reason: format!("{label} module graph is incomplete or invalid: {reason}"),
            };
        }
    }

    // Deletions may invalidate source-level references beyond explicit import
    // edges, especially until symbol-resolution dependencies are complete.
    if before
        .modules
        .keys()
        .any(|name| !after.modules.contains_key(name))
    {
        return Invalidation::FullRebuild {
            reason: "module removed".into(),
        };
    }

    let mut dirty = BTreeSet::new();
    for (name, module) in &after.modules {
        if before.modules.get(name) != Some(module) {
            dirty.insert(name.clone());
        }
    }
    if dirty.is_empty() {
        return Invalidation::Unchanged;
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
    Invalidation::Dirty {
        modules: dirty.into_iter().collect(),
    }
}

fn validate_graph(snapshot: &ModuleSnapshot) -> Result<(), String> {
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
    let mut visited = 0;
    while let Some(name) = ready.pop_first() {
        visited += 1;
        if let Some(dependents) = reverse.get(&name) {
            for dependent in dependents {
                let degree = indegrees.get_mut(dependent).expect("validated module");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(dependent.clone());
                }
            }
        }
    }
    if visited != snapshot.modules.len() {
        return Err("module dependency cycle".into());
    }
    Ok(())
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

    fn interfaces(snapshot: &ModuleSnapshot) -> ModuleInterfaces {
        let mut result = ModuleInterfaces::new(snapshot);
        for name in snapshot.modules.keys() {
            result.insert(name.clone(), name.as_bytes());
        }
        result
    }

    #[test]
    fn private_body_edit_does_not_invalidate_importers_interfaces() {
        let before = fixture(b"compiler=1");
        let mut after = before.clone();
        after.insert("core", b"fn core() { 42 }", [] as [&str; 0]);
        assert_eq!(
            analyze_interface_invalidation(
                &before,
                &after,
                &interfaces(&before),
                &interfaces(&after),
            ),
            InterfaceInvalidation::Dirty {
                changed_modules: vec!["core".into()],
                affected_dependents: vec![],
            }
        );
        assert_eq!(
            invalidated_modules(&before, &after),
            Invalidation::Dirty {
                modules: vec!["api".into(), "app".into(), "core".into()]
            }
        );
    }

    #[test]
    fn public_interface_change_invalidates_transitive_importers() {
        let before = fixture(b"compiler=1");
        let mut after = before.clone();
        after.insert("core", b"fn core(x) { x }", [] as [&str; 0]);
        let mut changed = interfaces(&after);
        changed.insert("core", b"core: Int -> Int");
        assert_eq!(
            analyze_interface_invalidation(&before, &after, &interfaces(&before), &changed),
            InterfaceInvalidation::Dirty {
                changed_modules: vec!["core".into()],
                affected_dependents: vec!["api".into(), "app".into()],
            }
        );
    }

    #[test]
    fn missing_interface_observation_is_full_rebuild() {
        let before = fixture(b"compiler=1");
        let mut incomplete = interfaces(&before);
        incomplete.fingerprints.remove("api");
        assert!(matches!(
            analyze_interface_invalidation(&before, &before, &interfaces(&before), &incomplete),
            InterfaceInvalidation::FullRebuild { .. }
        ));
    }

    #[test]
    fn changing_import_edges_invalidates_downstream_even_with_stable_api() {
        let before = fixture(b"compiler=1");
        let mut after = before.clone();
        after.insert("api", b"fn api() { core() }", ["core", "unrelated"]);
        assert_eq!(
            analyze_interface_invalidation(
                &before,
                &after,
                &interfaces(&before),
                &interfaces(&after),
            ),
            InterfaceInvalidation::Dirty {
                changed_modules: vec!["api".into()],
                affected_dependents: vec!["app".into()],
            }
        );
    }

    #[test]
    fn interface_change_without_source_change_is_reported() {
        let snapshot = fixture(b"compiler=1");
        let mut updated = interfaces(&snapshot);
        updated.insert("core", b"changed exported effect row");
        assert_eq!(
            analyze_interface_invalidation(&snapshot, &snapshot, &interfaces(&snapshot), &updated,),
            InterfaceInvalidation::Dirty {
                changed_modules: vec!["core".into()],
                affected_dependents: vec!["api".into(), "app".into()],
            }
        );
    }

    #[test]
    fn added_unimported_module_only_changes_itself() {
        let before = fixture(b"compiler=1");
        let mut after = before.clone();
        after.insert("new", b"fn new() { 1 }", [] as [&str; 0]);
        assert_eq!(
            analyze_interface_invalidation(
                &before,
                &after,
                &interfaces(&before),
                &interfaces(&after),
            ),
            InterfaceInvalidation::Dirty {
                changed_modules: vec!["new".into()],
                affected_dependents: vec![],
            }
        );
    }

    #[test]
    fn complete_equal_interface_observations_are_unchanged() {
        let before = fixture(b"compiler=1");
        assert_eq!(
            analyze_interface_invalidation(
                &before,
                &before,
                &interfaces(&before),
                &interfaces(&before),
            ),
            InterfaceInvalidation::Unchanged
        );
    }

    #[test]
    fn mismatched_interface_compilation_context_requires_full_rebuild() {
        let before = fixture(b"compiler=1");
        let mut observed = interfaces(&before);
        observed.context_digest = [0; 32];
        assert!(matches!(
            analyze_interface_invalidation(&before, &before, &interfaces(&before), &observed,),
            InterfaceInvalidation::FullRebuild { .. }
        ));
    }

    #[test]
    fn extra_interface_observation_requires_full_rebuild() {
        let before = fixture(b"compiler=1");
        let mut observed = interfaces(&before);
        observed.insert("unknown", b"untracked public interface");
        assert!(matches!(
            analyze_interface_invalidation(&before, &before, &interfaces(&before), &observed,),
            InterfaceInvalidation::FullRebuild { .. }
        ));
    }

    #[test]
    fn invalid_module_graph_remains_fail_closed() {
        let before = fixture(b"compiler=1");
        let mut after = before.clone();
        after.modules.remove("core");
        assert!(matches!(
            analyze_interface_invalidation(
                &before,
                &after,
                &interfaces(&before),
                &interfaces(&after),
            ),
            InterfaceInvalidation::FullRebuild { .. }
        ));
    }

    #[test]
    fn changed_compiler_context_remains_fail_closed() {
        let before = fixture(b"compiler=1");
        let after = fixture(b"compiler=2");
        assert!(matches!(
            analyze_interface_invalidation(
                &before,
                &after,
                &interfaces(&before),
                &interfaces(&after),
            ),
            InterfaceInvalidation::FullRebuild { .. }
        ));
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
