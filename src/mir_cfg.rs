//! Shared MIR control-flow analysis.
//!
//! This module intentionally models explicit MIR control flow only:
//! `Jump` and `Branch` terminators. Effect-handler dispatch is a separate
//! implicit edge system in the native backend; transforms that rely on this
//! dominance relation must conservatively opt out when those implicit edges
//! are semantically relevant.

use crate::mir::{BlockId, Function, Terminator};
use std::collections::{BTreeMap, BTreeSet};

/// Explicit normal-control-flow successors for every MIR block.
pub fn successors(function: &Function) -> BTreeMap<BlockId, Vec<BlockId>> {
    function
        .blocks
        .iter()
        .map(|block| {
            let targets = match &block.terminator {
                Terminator::Jump(target) => vec![*target],
                Terminator::Branch { then_, else_, .. } => {
                    if then_ == else_ {
                        vec![*then_]
                    } else {
                        vec![*then_, *else_]
                    }
                }
                Terminator::Return(_)
                | Terminator::Resume(_)
                | Terminator::Unterminated => Vec::new(),
            };
            (block.id, targets)
        })
        .collect()
}

/// Explicit normal-control-flow predecessors for every MIR block.
pub fn predecessors(function: &Function) -> BTreeMap<BlockId, Vec<BlockId>> {
    let mut out: BTreeMap<BlockId, Vec<BlockId>> = function
        .blocks
        .iter()
        .map(|block| (block.id, Vec::new()))
        .collect();

    for (source, targets) in successors(function) {
        for target in targets {
            let preds = out.entry(target).or_default();
            if !preds.contains(&source) {
                preds.push(source);
            }
        }
    }

    out
}

/// Reachable explicit-CFG blocks from the function entry.
pub fn reachable(function: &Function) -> BTreeSet<BlockId> {
    let succs = successors(function);
    let mut visited = BTreeSet::new();
    let mut stack = vec![function.entry];

    while let Some(block) = stack.pop() {
        if !visited.insert(block) {
            continue;
        }
        if let Some(next) = succs.get(&block) {
            stack.extend(next.iter().copied());
        }
    }

    visited
}

/// Iterative dominator solution over explicit MIR control flow.
///
/// `a` dominates `b` iff every explicit path from the function entry to
/// `b` contains `a`. Unreachable blocks are deliberately excluded.
#[derive(Debug, Clone)]
pub struct Dominators {
    reachable: BTreeSet<BlockId>,
    sets: BTreeMap<BlockId, BTreeSet<BlockId>>,
}

impl Dominators {
    pub fn compute(function: &Function) -> Self {
        let reachable = reachable(function);
        let preds = predecessors(function);
        let all = reachable.clone();
        let mut sets = BTreeMap::new();

        for block in &function.blocks {
            if !reachable.contains(&block.id) {
                continue;
            }
            if block.id == function.entry {
                sets.insert(block.id, BTreeSet::from([block.id]));
            } else {
                sets.insert(block.id, all.clone());
            }
        }

        loop {
            let mut changed = false;

            for block in &function.blocks {
                if block.id == function.entry || !reachable.contains(&block.id) {
                    continue;
                }

                let incoming: Vec<BlockId> = preds
                    .get(&block.id)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|pred| reachable.contains(pred))
                    .collect();

                let mut next = if let Some(first) = incoming.first() {
                    sets.get(first).cloned().unwrap_or_default()
                } else {
                    BTreeSet::new()
                };

                for pred in incoming.iter().skip(1) {
                    let pred_set = sets.get(pred).cloned().unwrap_or_default();
                    next = next.intersection(&pred_set).copied().collect();
                }
                next.insert(block.id);

                if sets.get(&block.id) != Some(&next) {
                    sets.insert(block.id, next);
                    changed = true;
                }
            }

            if !changed {
                break;
            }
        }

        Self { reachable, sets }
    }

    pub fn is_reachable(&self, block: BlockId) -> bool {
        self.reachable.contains(&block)
    }

    pub fn dominates(&self, dominator: BlockId, block: BlockId) -> bool {
        self.sets
            .get(&block)
            .is_some_and(|set| set.contains(&dominator))
    }

    pub fn strict_dominates(&self, dominator: BlockId, block: BlockId) -> bool {
        dominator != block && self.dominates(dominator, block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{FunctionBuilder, RValue};
    use crate::bytecode::Constant;
    use crate::types::Type;

    #[test]
    fn entry_dominates_both_sides_and_join() {
        let mut builder = FunctionBuilder::new("diamond", Some(Type::int()));
        let cond = builder.add_temp(Type::bool());
        builder.assign(cond, RValue::Const(Constant::Bool(true)));

        let left = builder.create_block();
        let right = builder.create_block();
        let join = builder.create_block();
        builder.terminate(Terminator::Branch {
            cond,
            then_: left,
            else_: right,
        });

        builder.switch_to(left);
        builder.terminate(Terminator::Jump(join));

        builder.switch_to(right);
        builder.terminate(Terminator::Jump(join));

        builder.switch_to(join);
        let result = builder.add_temp(Type::int());
        builder.assign(result, RValue::Const(Constant::Int(1)));
        builder.terminate(Terminator::Return(Some(result)));

        let function = builder.build();
        let dom = Dominators::compute(&function);

        assert!(dom.dominates(BlockId(0), left));
        assert!(dom.dominates(BlockId(0), right));
        assert!(dom.dominates(BlockId(0), join));
        assert!(!dom.dominates(left, join));
        assert!(!dom.dominates(right, join));
    }

    #[test]
    fn branch_local_block_dominates_its_descendant_only() {
        let mut builder = FunctionBuilder::new("branch", Some(Type::int()));
        let cond = builder.add_temp(Type::bool());
        builder.assign(cond, RValue::Const(Constant::Bool(true)));

        let left = builder.create_block();
        let right = builder.create_block();
        let left_child = builder.create_block();
        let join = builder.create_block();
        builder.terminate(Terminator::Branch {
            cond,
            then_: left,
            else_: right,
        });

        builder.switch_to(left);
        builder.terminate(Terminator::Jump(left_child));
        builder.switch_to(left_child);
        builder.terminate(Terminator::Jump(join));
        builder.switch_to(right);
        builder.terminate(Terminator::Jump(join));
        builder.switch_to(join);
        let result = builder.add_temp(Type::int());
        builder.assign(result, RValue::Const(Constant::Int(1)));
        builder.terminate(Terminator::Return(Some(result)));

        let function = builder.build();
        let dom = Dominators::compute(&function);

        assert!(dom.strict_dominates(left, left_child));
        assert!(!dom.dominates(left, join));
    }

    #[test]
    fn unreachable_block_is_not_in_dominator_relation() {
        let mut builder = FunctionBuilder::new("unreachable", None);
        builder.terminate(Terminator::Return(None));
        let dead = builder.create_block();
        builder.switch_to(dead);
        builder.terminate(Terminator::Return(None));

        let function = builder.build();
        let dom = Dominators::compute(&function);

        assert!(!dom.is_reachable(dead));
        assert!(!dom.dominates(function.entry, dead));
        assert!(!dom.dominates(dead, dead));
    }
}
