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

    #[test]
    fn long_dependency_chain_invalidates_iteratively_without_missing_dependents() {
        // Thousands of imports must not use the native call stack for graph
        // traversal. This also catches partial invalidation of deep chains.
        const MODULES: usize = 4_096;
        let mut before = ModuleSnapshot::new(b"fixed compiler context");
        for index in 0..MODULES {
            let name = format!("module_{index:05}");
            if index == 0 {
                before.insert(name, b"initial", [] as [&str; 0]);
            } else {
                before.insert(
                    name,
                    b"unchanged",
                    [format!("module_{:05}", index - 1)],
                );
            }
        }

        let mut after = before.clone();
        after.insert("module_00000", b"edited", [] as [&str; 0]);
        let expected = (0..MODULES)
            .map(|index| format!("module_{index:05}"))
            .collect::<Vec<_>>();
        assert_eq!(
            invalidated_modules(&before, &after),
            Invalidation::Dirty { modules: expected }
        );
    }

    #[test]
    fn layered_diamond_invalidates_each_module_once_despite_shared_paths() {
        // A layered diamond has exponentially many paths, but only O(n)
        // modules and import edges. Invalidation must operate on the graph,
        // not enumerate every path from the changed root.
        const LEVELS: usize = 96;
        let mut before = ModuleSnapshot::new(b"fixed compiler context");
        before.insert("root", b"initial", [] as [&str; 0]);
        before.insert("unrelated", b"unchanged", [] as [&str; 0]);
        for level in 0..LEVELS {
            let imports = if level == 0 {
                vec!["root".to_string()]
            } else {
                vec![
                    format!("left_{:03}", level - 1),
                    format!("right_{:03}", level - 1),
                ]
            };
            before.insert(format!("left_{level:03}"), b"unchanged", imports.clone());
            before.insert(format!("right_{level:03}"), b"unchanged", imports);
        }

        let mut after = before.clone();
        after.insert("root", b"edited", [] as [&str; 0]);
        let result = invalidated_modules(&before, &after);
        let Invalidation::Dirty { modules } = result else {
            panic!("complete valid graph must yield a dirty set");
        };
        assert_eq!(modules.len(), 1 + 2 * LEVELS);
        assert!(modules.contains(&"root".to_string()));
        assert!(modules.contains(&format!("left_{:03}", LEVELS - 1)));
        assert!(modules.contains(&format!("right_{:03}", LEVELS - 1)));
        assert!(!modules.contains(&"unrelated".to_string()));
    }

}
