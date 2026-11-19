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

/// Serialize a narrow, checked function-only interface using the compiler's
/// inferred type contracts and fixed-point effect rows.
///
/// Nulang's current cross-module visibility rules are not fully enforced, so
/// ALL top-level function declarations are represented, not just public ones.
/// Unsupported declarations fail closed rather than fabricating a partial API
/// fingerprint. Nested modules, imports, actors, classes, durable schemas,
/// and FFI need dedicated semantic treatment.
///
/// The caller must run the canonical compiler type and effect checks first.
/// This helper cannot authenticate that fact or prove dependency closure;
/// its output is diagnostic-only, never a cache reuse key.
pub fn canonical_checked_function_interface_bytes(
    ast: &crate::ast::AstModule,
    checked_types: &crate::typechecker::TypeChecker,
    checked_effects: &crate::effect_checker::EffectChecker,
) -> Result<Vec<u8>, String> {
    let mut signatures = BTreeMap::new();
    for decl in &ast.decls {
        match decl {
            crate::ast::Decl::Function {
                name,
                params,
                type_params,
                type_param_constraints,
                default_values,
                using_params,
                requires,
                ensures,
                annotations,
                public,
                cap: declared_cap,
                ..
            } => {
                if !type_param_constraints.is_empty()
                    || default_values.iter().any(Option::is_some)
                    || !using_params.is_empty()
                    || !requires.is_empty()
                    || !ensures.is_empty()
                    || !annotations.is_empty()
                {
                    return Err(format!(
                        "function '{name}' has defaults, constraints, contracts or annotations that this interface version does not encode"
                    ));
                }
                let inferred = checked_types
                    .inferred_decl_types
                    .get(name)
                    .ok_or_else(|| format!("missing inferred type for function '{name}'"))?;
                let crate::types::Type::Function {
                    param, ret, cap, ..
                } = inferred
                else {
                    return Err(format!("inferred type for '{name}' is not a function"));
                };
                let row = checked_effects
                    .function_row(name)
                    .ok_or_else(|| format!("missing checked effect row for '{name}'"))?;
                let complete = crate::types::Type::Function {
                    param: param.clone(),
                    ret: ret.clone(),
                    effect: row.clone(),
                    cap: *cap,
                };
                let mut contract = vec![u8::from(*public)];
                // Preserve explicitly declared authority/capability contracts,
                // even when a downstream inferred type omits their provenance.
                match declared_cap {
                    Some(cap) => {
                        contract.push(1);
                        contract.push(crate::types::canonical_cap_code(cap));
                    }
                    None => contract.push(0),
                }
                contract.extend_from_slice(&(type_params.len() as u64).to_le_bytes());
                for parameter in type_params {
                    contract.extend_from_slice(&(parameter.len() as u64).to_le_bytes());
                    contract.extend_from_slice(parameter.as_bytes());
                }
                contract.extend_from_slice(&(params.len() as u64).to_le_bytes());
                for parameter in params {
                    contract.extend_from_slice(&(parameter.name.len() as u64).to_le_bytes());
                    contract.extend_from_slice(parameter.name.as_bytes());
                    match parameter.cap {
                        Some(cap) => {
                            contract.push(1);
                            contract.push(crate::types::canonical_cap_code(&cap));
                        }
                        None => contract.push(0),
                    }
                }
                contract.extend_from_slice(&crate::semantic_schema::canonical_type_schema_bytes(
                    &complete,
                ));
                if signatures.insert(name.clone(), contract).is_some() {
                    return Err(format!("duplicate function name '{name}'"));
                }
            }
            _ => {
                return Err(
                    "checked function interface cannot represent non-function declarations".into(),
                );
            }
        }
    }

    let mut out = b"nulang.checked-function-interface.v1\0".to_vec();
    out.extend_from_slice(&(signatures.len() as u64).to_le_bytes());
    for (name, bytes) in signatures {
        for part in [name.as_bytes(), bytes.as_slice()] {
            out.extend_from_slice(&(part.len() as u64).to_le_bytes());
            out.extend_from_slice(part);
        }
    }
    Ok(out)
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

    fn checked_interface(source: &str) -> Result<Vec<u8>, String> {
        let tokens = crate::lexer::Lexer::new(source)
            .lex()
            .map_err(|e| format!("{e:?}"))?;
        let mut parser = crate::parser::Parser::new(tokens);
        let ast = parser.parse_module().map_err(|e| format!("{e:?}"))?;
        let mut typechecker = crate::typechecker::TypeChecker::new();
        typechecker
            .check_module(&ast)
            .map_err(|e| format!("{e:?}"))?;
        let mut effects = crate::effect_checker::EffectChecker::new();
        effects
            .check_module(&ast.decls)
            .map_err(|e| format!("{e:?}"))?;
        canonical_checked_function_interface_bytes(&ast, &typechecker, &effects)
    }

    #[test]
    fn checked_private_implementation_change_keeps_function_interface() {
        let before = checked_interface("fn value() { 1 }").unwrap();
        let after = checked_interface("fn value() { 42 }").unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn checked_inferred_return_type_change_alters_function_interface() {
        let before = checked_interface("fn value() { 1 }").unwrap();
        let after = checked_interface("fn value() { \"text\" }").unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn checked_inferred_effect_change_alters_function_interface() {
        let before = checked_interface("fn value() { 1 }").unwrap();
        let after = checked_interface("fn value() { perform IO.print(\"x\") 1 }").unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn unsupported_declarations_fail_closed_in_checked_interface() {
        let result = checked_interface("type NewType = Int");
        assert!(
            result.is_err(),
            "type declarations must not silently disappear"
        );
    }

    #[test]
    fn private_callee_effect_change_propagates_into_checked_contracts() {
        let pure = checked_interface("fn helper() { 1 }\nfn value() { helper() }").unwrap();
        let effectful =
            checked_interface("fn helper() { perform IO.print(\"x\") 1 }\nfn value() { helper() }")
                .unwrap();
        assert_ne!(pure, effectful);
    }

    #[test]
    fn checked_function_order_does_not_change_interface_bytes() {
        let a = checked_interface("fn alpha() { 1 }\nfn beta() { 2 }").unwrap();
        let b = checked_interface("fn beta() { 2 }\nfn alpha() { 1 }").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_checked_type_encoding_alpha_normalizes_inference_variables() {
        use crate::types::{Capability, EffectRow, Type, TypeVar};
        let function_type = |id| Type::Function {
            param: Box::new(Type::Var(TypeVar(id))),
            ret: Box::new(Type::Var(TypeVar(id))),
            effect: EffectRow::empty(),
            cap: Capability::Ref,
        };
        assert_eq!(
            crate::semantic_schema::canonical_type_schema_bytes(&function_type(2)),
            crate::semantic_schema::canonical_type_schema_bytes(&function_type(900)),
        );
    }

    #[test]
    fn unchanged_interface_does_not_prove_generated_code_is_reusable() {
        let first = "fn value() { 1 }";
        let second = "fn value() { 42 }";
        assert_eq!(
            checked_interface(first).unwrap(),
            checked_interface(second).unwrap()
        );

        let compile = |source: &str| {
            let tokens = crate::lexer::Lexer::new(source).lex().unwrap();
            let ast = crate::parser::Parser::new(tokens).parse_module().unwrap();
            let mut types = crate::typechecker::TypeChecker::new();
            types.check_module(&ast).unwrap();
            let hir = crate::hir_lower::lower_module(&ast, &types.inferred_decl_types);
            let mut mir = crate::mir_lower::lower_module(&hir).unwrap();
            crate::mir_codegen::compile_mir(&mut mir, "contract-test").unwrap()
        };
        let old_code = compile(first);
        let new_code = compile(second);
        assert_ne!(
            format!("{:?}", old_code),
            format!("{:?}", new_code),
            "stable function signatures must not authorize reuse of stale executable artifacts"
        );
    }

    #[test]
    fn unsupported_function_contract_metadata_is_rejected() {
        let source = "fn value() { 1 }";
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(source).lex().unwrap())
            .parse_module()
            .unwrap();
        let mut types = crate::typechecker::TypeChecker::new();
        types.check_module(&ast).unwrap();
        let mut effects = crate::effect_checker::EffectChecker::new();
        effects.check_module(&ast.decls).unwrap();
        let mut constrained = ast.clone();
        if let crate::ast::Decl::Function { requires, .. } = &mut constrained.decls[0] {
            requires.push(crate::ast::Expr::Literal(
                crate::ast::Literal::Bool(true),
                crate::types::Span::default(),
            ));
        }
        assert!(
            canonical_checked_function_interface_bytes(&constrained, &types, &effects).is_err()
        );
    }

    #[test]
    fn explicit_parameter_capability_changes_checked_contract() {
        let source = "fn value(x: Int) { x }";
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(source).lex().unwrap())
            .parse_module()
            .unwrap();
        let mut types = crate::typechecker::TypeChecker::new();
        types.check_module(&ast).unwrap();
        let mut effects = crate::effect_checker::EffectChecker::new();
        effects.check_module(&ast.decls).unwrap();
        let mut different = ast.clone();
        if let crate::ast::Decl::Function { params, .. } = &mut different.decls[0] {
            params[0].cap = Some(crate::types::Capability::Iso);
        }
        assert_ne!(
            canonical_checked_function_interface_bytes(&ast, &types, &effects).unwrap(),
            canonical_checked_function_interface_bytes(&different, &types, &effects).unwrap(),
        );
    }

    #[test]
    fn public_visibility_changes_checked_contract() {
        let source = "fn value() { 1 }";
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(source).lex().unwrap())
            .parse_module()
            .unwrap();
        let mut types = crate::typechecker::TypeChecker::new();
        types.check_module(&ast).unwrap();
        let mut effects = crate::effect_checker::EffectChecker::new();
        effects.check_module(&ast.decls).unwrap();
        let mut exported = ast.clone();
        if let crate::ast::Decl::Function { public, .. } = &mut exported.decls[0] {
            *public = true;
        }
        assert_ne!(
            canonical_checked_function_interface_bytes(&ast, &types, &effects).unwrap(),
            canonical_checked_function_interface_bytes(&exported, &types, &effects).unwrap(),
        );
    }

    #[test]
    fn compiler_checked_interface_connects_to_module_planner() {
        let first = "fn value() { 1 }";
        let second = "fn value() { 42 }";
        let before = fixture(b"compiler=1");
        let mut after = before.clone();
        after.insert("core", second.as_bytes(), [] as [&str; 0]);
        let mut old = interfaces(&before);
        old.insert("core", &checked_interface(first).unwrap());
        let mut new = interfaces(&after);
        new.insert("core", &checked_interface(second).unwrap());
        assert_eq!(
            analyze_interface_invalidation(&before, &after, &old, &new),
            InterfaceInvalidation::Dirty {
                changed_modules: vec!["core".into()],
                affected_dependents: vec![],
            }
        );
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
