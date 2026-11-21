//! Stable compiler-owned identity for explicit effect sites in lowered MIR.
//!
//! A durable effect occurrence needs two different identities:
//!
//! - the owning definition's `SemanticId`, which answers "which program
//!   semantics are executing?"; and
//! - an `EffectSiteId`, which answers "which explicit effect occurrence inside
//!   those semantics is this?".
//!
//! This module defines the second identity. Site IDs deliberately exclude source
//! spans, line-table entries, raw MIR block IDs, and declaration-vector indices.
//! They are derived from the semantic owner, a normalized MIR block/statement
//! coordinate, a stable nested-rvalue path, and the qualified effect operation.
//!
//! An `EffectSiteId` is therefore a locator, not a substitute for
//! `SemanticId`. Durable invocation identity should combine both. If the
//! owning definition's semantics change, compatibility must still be decided by
//! semantic identity/migration rules even when an individual site ID happens to
//! remain unchanged.

use blake3::Hasher;
use std::collections::BTreeMap;
use std::fmt;

use crate::mir::{self, RValue, Stmt};

const EFFECT_SITE_DOMAIN: &[u8] = b"nulang.effect-site.v1\0";

/// Stable identity for one explicit effect occurrence in canonicalized MIR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectSiteId([u8; 32]);

impl EffectSiteId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    fn derive(
        module_name: &str,
        owner_kind: EffectSiteOwnerKind,
        owner_name: &str,
        canonical_block: u32,
        statement_index: u32,
        nested_path: &[String],
        effect_operation: &str,
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(EFFECT_SITE_DOMAIN);
        put_bytes(&mut hasher, module_name.as_bytes());
        hasher.update(&[owner_kind as u8]);
        put_bytes(&mut hasher, owner_name.as_bytes());
        hasher.update(&canonical_block.to_le_bytes());
        hasher.update(&statement_index.to_le_bytes());
        hasher.update(&(nested_path.len() as u64).to_le_bytes());
        for component in nested_path {
            put_bytes(&mut hasher, component.as_bytes());
        }
        put_bytes(&mut hasher, effect_operation.as_bytes());
        Self(*hasher.finalize().as_bytes())
    }
}

impl fmt::Display for EffectSiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// Semantic owner category used in the site-id domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EffectSiteOwnerKind {
    Function = 0,
    Behavior = 1,
}

/// Compiler-visible description of one explicit performed effect site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectSite {
    pub id: EffectSiteId,
    pub owner_kind: EffectSiteOwnerKind,
    /// Stable semantic owner name. Behaviors are qualified by their owning
    /// actor/workflow when that ownership is available in MIR metadata.
    pub owner_name: String,
    /// Qualified operation identity, e.g. `IO.print`, `Inference.ask`, or
    /// `Signal.wait`.
    pub effect_operation: String,
    /// Dense block identity after normalizing raw MIR block IDs.
    pub canonical_block: u32,
    /// Statement index inside the canonical block.
    pub statement_index: u32,
    /// Stable path for effects nested inside another MIR rvalue (currently
    /// spawn initializer fields).
    pub nested_path: Vec<String>,
}

/// Enumerate compiler-owned effect-site identities for a lowered MIR module.
///
/// Output order is deterministic and independent of top-level declaration
/// vector order. This function covers explicit `perform` forms represented by
/// `Perform`, `PerformAsync`, and the specialized `SignalWait` lowering.
pub fn effect_sites_for_mir(module: &mir::Module) -> Vec<EffectSite> {
    let behavior_owners = behavior_owner_names(module);
    let mut sites = Vec::new();

    let mut functions: Vec<_> = module.functions.iter().collect();
    functions.sort_by(|left, right| left.name.cmp(&right.name));
    for function in functions {
        collect_function_sites(
            module,
            EffectSiteOwnerKind::Function,
            &function.name,
            function,
            &mut sites,
        );
    }

    let mut behaviors: Vec<_> = module.behaviors.iter().enumerate().collect();
    behaviors.sort_by(|(left_index, left), (right_index, right)| {
        behavior_owners[*left_index]
            .cmp(&behavior_owners[*right_index])
            .then_with(|| left.name.cmp(&right.name))
    });
    for (index, behavior) in behaviors {
        collect_function_sites(
            module,
            EffectSiteOwnerKind::Behavior,
            &behavior_owners[index],
            behavior,
            &mut sites,
        );
    }

    sites.sort_by(|left, right| {
        left.owner_kind
            .cmp(&right.owner_kind)
            .then_with(|| left.owner_name.cmp(&right.owner_name))
            .then_with(|| left.canonical_block.cmp(&right.canonical_block))
            .then_with(|| left.statement_index.cmp(&right.statement_index))
            .then_with(|| left.nested_path.cmp(&right.nested_path))
            .then_with(|| left.effect_operation.cmp(&right.effect_operation))
    });
    sites
}

fn collect_function_sites(
    module: &mir::Module,
    owner_kind: EffectSiteOwnerKind,
    owner_name: &str,
    function: &mir::Function,
    out: &mut Vec<EffectSite>,
) {
    let block_ids = canonical_block_ids(function);
    let mut blocks: Vec<_> = function.blocks.iter().collect();
    blocks.sort_by_key(|block| block_ids.get(&block.id.0).copied().unwrap_or(block.id.0));

    for block in blocks {
        let canonical_block = block_ids.get(&block.id.0).copied().unwrap_or(block.id.0);
        for (statement_index, stmt) in block.stmts.iter().enumerate() {
            if let Stmt::Assign { op, .. } = stmt {
                let mut nested_path = Vec::new();
                collect_rvalue_sites(
                    module,
                    owner_kind,
                    owner_name,
                    canonical_block,
                    statement_index as u32,
                    &mut nested_path,
                    op,
                    out,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_rvalue_sites(
    module: &mir::Module,
    owner_kind: EffectSiteOwnerKind,
    owner_name: &str,
    canonical_block: u32,
    statement_index: u32,
    nested_path: &mut Vec<String>,
    value: &RValue,
    out: &mut Vec<EffectSite>,
) {
    let operation = match value {
        RValue::Perform { effect, op, .. } => Some(format!("{effect}.{op}")),
        RValue::PerformAsync { effect_op, .. } => Some(effect_op.clone()),
        RValue::SignalWait { .. } => Some("Signal.wait".to_string()),
        _ => None,
    };

    if let Some(effect_operation) = operation {
        let id = EffectSiteId::derive(
            &module.name,
            owner_kind,
            owner_name,
            canonical_block,
            statement_index,
            nested_path,
            &effect_operation,
        );
        out.push(EffectSite {
            id,
            owner_kind,
            owner_name: owner_name.to_string(),
            effect_operation,
            canonical_block,
            statement_index,
            nested_path: nested_path.clone(),
        });
    }

    // Spawn initializer expressions are nested MIR rvalues. Field order is not
    // semantic, so traverse by field name rather than source/vector position.
    if let RValue::Spawn { init, .. } = value {
        let mut fields: Vec<_> = init.iter().collect();
        fields.sort_by(|left, right| left.0.cmp(&right.0));
        for (field, nested) in fields {
            nested_path.push(format!("spawn-init:{field}"));
            collect_rvalue_sites(
                module,
                owner_kind,
                owner_name,
                canonical_block,
                statement_index,
                nested_path,
                nested,
                out,
            );
            nested_path.pop();
        }
    }
}

fn canonical_block_ids(function: &mir::Function) -> BTreeMap<u32, u32> {
    let mut raw: Vec<_> = function.blocks.iter().map(|block| block.id.0).collect();
    raw.sort_unstable();
    raw.dedup();
    raw.into_iter()
        .enumerate()
        .map(|(index, id)| (id, index as u32))
        .collect()
}

fn behavior_owner_names(module: &mir::Module) -> Vec<String> {
    module
        .behaviors
        .iter()
        .enumerate()
        .map(|(behavior_index, behavior)| {
            let mut owners: Vec<_> = module
                .actor_metadata
                .iter()
                .filter(|actor| actor.behavior_indices.contains(&behavior_index))
                .map(|actor| actor.name.as_str())
                .collect();
            owners.sort_unstable();
            owners.dedup();

            match owners.as_slice() {
                [] => behavior.name.clone(),
                [actor] => format!("{actor}::{}", behavior.name),
                _ => format!("{}::{}", owners.join("+"), behavior.name),
            }
        })
        .collect()
}

fn put_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir_lower;
    use crate::lexer::Lexer;
    use crate::mir_lower;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;

    fn lower(source: &str) -> mir::Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();
        let mut typechecker = TypeChecker::new();
        typechecker.check_module(&ast).unwrap();
        let hir = hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
        mir_lower::lower_module(&hir).unwrap()
    }

    #[test]
    fn formatting_and_comments_do_not_change_effect_site_id() {
        let compact = lower(
            "effect Clock { now: -> Int }\nfn read() -> Int ! {Clock} { perform Clock.now() }\nread()",
        );
        let formatted = lower(
            "// presentation only\n\neffect Clock { now: -> Int }\n\nfn read() -> Int ! {Clock} {\n    // still the same effect site\n    perform Clock.now()\n}\n\nread()\n",
        );

        let compact_sites = effect_sites_for_mir(&compact);
        let formatted_sites = effect_sites_for_mir(&formatted);
        assert_eq!(compact_sites.len(), 1);
        assert_eq!(formatted_sites.len(), 1);
        assert_eq!(compact_sites[0].id, formatted_sites[0].id);
        assert_eq!(compact_sites[0].effect_operation, "Clock.now");
    }

    #[test]
    fn top_level_declaration_reordering_does_not_change_effect_site_id() {
        let first = lower(
            "effect Clock { now: -> Int }\nfn helper() -> Int { 7 }\nfn read() -> Int ! {Clock} { perform Clock.now() }\nread()",
        );
        let reordered = lower(
            "effect Clock { now: -> Int }\nfn read() -> Int ! {Clock} { perform Clock.now() }\nfn helper() -> Int { 7 }\nread()",
        );

        let first_site = effect_sites_for_mir(&first)
            .into_iter()
            .find(|site| site.owner_name == "read")
            .unwrap();
        let reordered_site = effect_sites_for_mir(&reordered)
            .into_iter()
            .find(|site| site.owner_name == "read")
            .unwrap();

        assert_eq!(first_site.id, reordered_site.id);
    }

    #[test]
    fn repeated_same_operation_in_one_owner_has_distinct_site_ids() {
        let module = lower(
            "effect Clock { now: -> Int }\nfn read_twice() -> Int ! {Clock} { let a = perform Clock.now(); let b = perform Clock.now(); a + b }\nread_twice()",
        );
        let sites: Vec<_> = effect_sites_for_mir(&module)
            .into_iter()
            .filter(|site| site.owner_name == "read_twice")
            .collect();

        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].effect_operation, "Clock.now");
        assert_eq!(sites[1].effect_operation, "Clock.now");
        assert_ne!(sites[0].id, sites[1].id);
    }

    #[test]
    fn operation_change_changes_effect_site_id() {
        let clock = lower(
            "effect Clock { now: -> Int }\nfn read() -> Int ! {Clock} { perform Clock.now() }\nread()",
        );
        let rng = lower(
            "effect Rng { next: -> Int }\nfn read() -> Int ! {Rng} { perform Rng.next() }\nread()",
        );

        let clock_site = effect_sites_for_mir(&clock)
            .into_iter()
            .find(|site| site.owner_name == "read")
            .unwrap();
        let rng_site = effect_sites_for_mir(&rng)
            .into_iter()
            .find(|site| site.owner_name == "read")
            .unwrap();

        assert_ne!(clock_site.id, rng_site.id);
    }
}
