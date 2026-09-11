//! Backward liveness analysis for MIR locals.
//!
//! The first consumer is continuation optimization: a single-shot algebraic
//! effect only needs to preserve locals that are live after the `Perform`.
//! The effect result destination is deliberately excluded from that capture
//! set because `resume(value)` overwrites it before continuation execution
//! continues.

use crate::mir::{BlockId, FuncRef, Function, LocalId, RValue, Stmt, Terminator};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A deterministic set of live MIR locals.
pub type LiveSet = BTreeSet<LocalId>;

/// Fixed-point liveness facts for one MIR function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Liveness {
    /// Locals live on entry to each block.
    pub block_live_in: BTreeMap<BlockId, LiveSet>,
    /// Locals live immediately before each statement.
    pub stmt_live_before: BTreeMap<(BlockId, usize), LiveSet>,
    /// Locals live immediately after each statement.
    pub stmt_live_after: BTreeMap<(BlockId, usize), LiveSet>,
}

impl Liveness {
    pub fn live_before(&self, block: BlockId, stmt_index: usize) -> Option<&LiveSet> {
        self.stmt_live_before.get(&(block, stmt_index))
    }

    pub fn live_after(&self, block: BlockId, stmt_index: usize) -> Option<&LiveSet> {
        self.stmt_live_after.get(&(block, stmt_index))
    }

    /// Locals whose *current values* must survive a `Perform` suspension.
    ///
    /// The assigned destination is excluded even when it is live afterwards:
    /// its pre-perform value is dead because `resume(value)` supplies the new
    /// value observed by the continuation.
    pub fn continuation_capture_set(
        &self,
        function: &Function,
        block: BlockId,
        stmt_index: usize,
    ) -> Option<LiveSet> {
        let stmt = function
            .blocks
            .iter()
            .find(|candidate| candidate.id == block)?
            .stmts
            .get(stmt_index)?;
        let Stmt::Assign { dst, op } = stmt else {
            return None;
        };
        if !matches!(op, RValue::Perform { .. }) {
            return None;
        }
        let mut live = self.live_after(block, stmt_index)?.clone();
        live.remove(dst);
        Some(live)
    }
}

/// Compute classical backwards liveness for a MIR function.
pub fn analyze(function: &Function) -> Liveness {
    let block_index: HashMap<BlockId, usize> = function
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| (block.id, index))
        .collect();

    let mut live_in: BTreeMap<BlockId, LiveSet> = function
        .blocks
        .iter()
        .map(|block| (block.id, LiveSet::new()))
        .collect();

    loop {
        let mut changed = false;

        for block in function.blocks.iter().rev() {
            let mut live = successor_live_in(&block.terminator, &live_in, &block_index);
            add_terminator_uses(&block.terminator, &mut live);

            for stmt in block.stmts.iter().rev() {
                transfer_stmt(stmt, &mut live);
            }

            let entry = live_in.entry(block.id).or_default();
            if *entry != live {
                *entry = live;
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    let mut result = Liveness {
        block_live_in: live_in.clone(),
        ..Liveness::default()
    };

    for block in &function.blocks {
        let mut live = successor_live_in(&block.terminator, &live_in, &block_index);
        add_terminator_uses(&block.terminator, &mut live);

        for (stmt_index, stmt) in block.stmts.iter().enumerate().rev() {
            result
                .stmt_live_after
                .insert((block.id, stmt_index), live.clone());
            transfer_stmt(stmt, &mut live);
            result
                .stmt_live_before
                .insert((block.id, stmt_index), live.clone());
        }
    }

    result
}

fn successor_live_in(
    terminator: &Terminator,
    live_in: &BTreeMap<BlockId, LiveSet>,
    block_index: &HashMap<BlockId, usize>,
) -> LiveSet {
    let mut live = LiveSet::new();
    let mut add_successor = |id: BlockId| {
        if block_index.contains_key(&id) {
            if let Some(successor_live) = live_in.get(&id) {
                live.extend(successor_live.iter().copied());
            }
        }
    };

    match terminator {
        Terminator::Jump(target) => add_successor(*target),
        Terminator::Branch { then_, else_, .. } => {
            add_successor(*then_);
            add_successor(*else_);
        }
        Terminator::Return(_) | Terminator::Resume(_) | Terminator::Unterminated => {}
    }

    live
}

fn transfer_stmt(stmt: &Stmt, live: &mut LiveSet) {
    for def in stmt_defs(stmt) {
        live.remove(&def);
    }
    add_stmt_uses(stmt, live);
}

fn stmt_defs(stmt: &Stmt) -> LiveSet {
    let mut defs = LiveSet::new();
    if let Stmt::Assign { dst, op } = stmt {
        defs.insert(*dst);
        // ReceiveMatch/ReceiveWait write payload values into the locals
        // immediately following dst. MIR lowering guarantees that contiguous
        // layout, so those implicit writes are real definitions too.
        let max_params = match op {
            RValue::ReceiveMatch { max_params, .. } | RValue::ReceiveWait { max_params, .. } => {
                *max_params
            }
            _ => 0,
        };
        for offset in 1..=max_params {
            defs.insert(LocalId(dst.0 + offset as u32));
        }
    }
    defs
}

fn add_stmt_uses(stmt: &Stmt, live: &mut LiveSet) {
    match stmt {
        Stmt::Assign { op, .. } => add_rvalue_uses(op, live),
        Stmt::StoreFieldNamed { obj, src, .. } => {
            live.insert(*obj);
            live.insert(*src);
        }
        Stmt::ArrayStore { arr, idx, src } => {
            live.insert(*arr);
            live.insert(*idx);
            live.insert(*src);
        }
        Stmt::EnterHandle { .. } | Stmt::PopHandler => {}
        Stmt::Emit { args, .. } => live.extend(args.iter().copied()),
        Stmt::StateSet { src, .. } => {
            live.insert(*src);
        }
    }
}

fn add_rvalue_uses(op: &RValue, live: &mut LiveSet) {
    match op {
        RValue::Const(_)
        | RValue::Panic(_)
        | RValue::SignalWait { .. }
        | RValue::Receive
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveCommit
        | RValue::SelfRef
        | RValue::StateGet { .. } => {}
        RValue::Load(local) | RValue::ArrayLen(local) | RValue::Resume(local) => {
            live.insert(*local);
        }
        RValue::LoadFieldNamed { obj, .. } | RValue::LoadFieldPos { obj, .. } => {
            live.insert(*obj);
        }
        RValue::ArrayLoad { arr, idx } => {
            live.insert(*arr);
            live.insert(*idx);
        }
        RValue::ArrayLit(items) | RValue::Tuple(items) => live.extend(items.iter().copied()),
        RValue::Unary(_, local) => {
            live.insert(*local);
        }
        RValue::Binary(_, left, right)
        | RValue::StringEq(left, right)
        | RValue::StrConcat(left, right) => {
            live.insert(*left);
            live.insert(*right);
        }
        RValue::Call { func, args } => {
            if let FuncRef::Local(local) = func {
                live.insert(*local);
            }
            live.extend(args.iter().copied());
        }
        RValue::Closure { captures, .. } => live.extend(captures.iter().copied()),
        RValue::Record(fields) => live.extend(fields.iter().map(|(_, local)| *local)),
        RValue::RecordUpdate { base, overrides } => {
            live.insert(*base);
            live.extend(overrides.iter().map(|(_, local)| *local));
        }
        RValue::Perform { args, .. }
        | RValue::PerformAsync { args, .. }
        | RValue::FFICall { args, .. } => live.extend(args.iter().copied()),
        RValue::ReceiveWait { timeout, .. } => {
            live.insert(*timeout);
        }
        RValue::Migrate { actor, node } => {
            live.insert(*actor);
            live.insert(*node);
        }
        RValue::CapabilityCheck { val } => {
            live.insert(*val);
        }
        RValue::Spawn {
            init, target_node, ..
        } => {
            for (_, value) in init {
                add_rvalue_uses(value, live);
            }
            if let Some(node) = target_node {
                live.insert(*node);
            }
        }
        RValue::Send { actor, args, .. } | RValue::Ask { actor, args, .. } => {
            live.insert(*actor);
            live.extend(args.iter().copied());
        }
    }
}

fn add_terminator_uses(terminator: &Terminator, live: &mut LiveSet) {
    match terminator {
        Terminator::Return(Some(local)) | Terminator::Resume(local) => {
            live.insert(*local);
        }
        Terminator::Branch { cond, .. } => {
            live.insert(*cond);
        }
        Terminator::Return(None) | Terminator::Jump(_) | Terminator::Unterminated => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{Block, HandlerRef, Module};

    fn function(blocks: Vec<Block>) -> Function {
        Function {
            name: "liveness_test".into(),
            params: Vec::new(),
            captures: Vec::new(),
            ret: None,
            locals: Vec::new(),
            blocks,
            entry: BlockId(0),
            handler_tables: Vec::new(),
            type_metadata: crate::type_metadata::TypeMetadata::default(),
            line_table: Vec::new(),
            placement: None,
        }
    }

    fn perform(args: Vec<LocalId>) -> RValue {
        RValue::Perform {
            effect: "State".into(),
            op: "get".into(),
            args,
            resolved_handler: Some(HandlerRef {
                table_index: 0,
                binding_index: 0,
            }),
        }
    }

    #[test]
    fn continuation_capture_keeps_only_values_live_after_perform() {
        let f = function(vec![Block {
            id: BlockId(0),
            stmts: vec![
                Stmt::Assign {
                    dst: LocalId(2),
                    op: perform(vec![LocalId(0)]),
                },
                Stmt::Assign {
                    dst: LocalId(3),
                    op: RValue::Load(LocalId(1)),
                },
            ],
            terminator: Terminator::Return(Some(LocalId(3))),
        }]);

        let liveness = analyze(&f);
        let captured = liveness
            .continuation_capture_set(&f, BlockId(0), 0)
            .expect("perform statement");
        assert_eq!(captured, LiveSet::from([LocalId(1)]));
    }

    #[test]
    fn continuation_capture_does_not_preserve_old_result_destination() {
        let f = function(vec![Block {
            id: BlockId(0),
            stmts: vec![
                Stmt::Assign {
                    dst: LocalId(2),
                    op: perform(vec![LocalId(0)]),
                },
                Stmt::Assign {
                    dst: LocalId(3),
                    op: RValue::Load(LocalId(2)),
                },
            ],
            terminator: Terminator::Return(Some(LocalId(3))),
        }]);

        let liveness = analyze(&f);
        assert_eq!(
            liveness
                .continuation_capture_set(&f, BlockId(0), 0)
                .expect("perform statement"),
            LiveSet::new()
        );
    }

    #[test]
    fn branch_successors_are_unioned() {
        let f = function(vec![
            Block {
                id: BlockId(0),
                stmts: vec![Stmt::Assign {
                    dst: LocalId(4),
                    op: perform(Vec::new()),
                }],
                terminator: Terminator::Branch {
                    cond: LocalId(0),
                    then_: BlockId(1),
                    else_: BlockId(2),
                },
            },
            Block {
                id: BlockId(1),
                stmts: Vec::new(),
                terminator: Terminator::Return(Some(LocalId(1))),
            },
            Block {
                id: BlockId(2),
                stmts: Vec::new(),
                terminator: Terminator::Return(Some(LocalId(2))),
            },
        ]);

        let liveness = analyze(&f);
        assert_eq!(
            liveness
                .continuation_capture_set(&f, BlockId(0), 0)
                .expect("perform statement"),
            LiveSet::from([LocalId(0), LocalId(1), LocalId(2)])
        );
    }

    #[test]
    fn receive_payload_locals_are_implicit_definitions() {
        let f = function(vec![Block {
            id: BlockId(0),
            stmts: vec![
                Stmt::Assign {
                    dst: LocalId(0),
                    op: RValue::ReceiveMatch {
                        behavior_ids: vec![1],
                        max_params: 2,
                    },
                },
                Stmt::Assign {
                    dst: LocalId(3),
                    op: RValue::Load(LocalId(1)),
                },
            ],
            terminator: Terminator::Return(Some(LocalId(3))),
        }]);

        let liveness = analyze(&f);
        assert!(!liveness.block_live_in[&BlockId(0)].contains(&LocalId(1)));
        assert!(liveness.live_after(BlockId(0), 0).unwrap().contains(&LocalId(1)));
    }

    #[test]
    fn nested_spawn_initializers_contribute_uses() {
        let f = function(vec![Block {
            id: BlockId(0),
            stmts: vec![Stmt::Assign {
                dst: LocalId(5),
                op: RValue::Spawn {
                    behavior_idx: 0,
                    init: vec![("x".into(), RValue::Load(LocalId(2)))],
                    target_node: Some(LocalId(3)),
                    capabilities: Vec::new(),
                },
            }],
            terminator: Terminator::Return(None),
        }]);

        let liveness = analyze(&f);
        assert_eq!(
            liveness.live_before(BlockId(0), 0).unwrap(),
            &LiveSet::from([LocalId(2), LocalId(3)])
        );
    }

    #[test]
    fn module_import_stays_unused_in_test_fixture() {
        // Keep this module-level smoke construction near the analysis tests so
        // future MIR shape additions are caught by exhaustive matches above.
        let module = Module {
            name: "test".into(),
            functions: Vec::new(),
            behaviors: Vec::new(),
            actor_metadata: Vec::new(),
            compensation_of: Vec::new(),
            parallel_branches_of: Vec::new(),
            foreign_functions: Vec::new(),
        };
        assert!(module.functions.is_empty());
    }
}
