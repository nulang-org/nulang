//! Backend-neutral continuation and suspension analysis over MIR.
//!
//! This module owns the semantic classification of continuation boundaries and
//! the liveness information needed to preserve a function across them. Native
//! AOT, WasmFX, and future backends should consume this analysis instead of
//! re-deriving suspension rules independently.

use crate::mir::{self, BlockId, LocalId, RValue, Stmt, Terminator};
use std::collections::{BTreeSet, HashMap};

/// Runtime scheduler suspension kinds.
///
/// These boundaries can leave the current actor turn and therefore require a
/// resumable execution state before a native backend may yield to the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerSuspendKind {
    AsyncEffect,
    SignalWait,
    ReceiveWait,
    LlmAsk,
}

/// Why a continuation must be preserved at a MIR statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationKind {
    /// Suspension transfers control to the actor/runtime scheduler.
    Scheduler(SchedulerSuspendKind),
    /// A statically resolved algebraic-effect handler may resume the
    /// continuation. `single_shot` is the compiler-proven multiplicity.
    ResumingEffect {
        handler_body: BlockId,
        single_shot: bool,
    },
}

/// One MIR continuation boundary plus the values needed to resume it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationSite {
    pub block: BlockId,
    pub stmt_index: usize,
    pub kind: ContinuationKind,
    /// Locals produced by the operation when execution resumes.
    ///
    /// Most boundaries define only the assignment destination. Selective
    /// receive also defines the contiguous payload locals following it.
    pub resume_defs: Vec<LocalId>,
    /// Values from before the boundary that are read after resumption before
    /// being redefined. Sorted for deterministic frame layouts/codegen.
    pub live_across: Vec<LocalId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContinuationAnalysis {
    pub sites: Vec<ContinuationSite>,
}

impl ContinuationAnalysis {
    pub fn has_scheduler_suspension(&self) -> bool {
        self.sites
            .iter()
            .any(|site| matches!(site.kind, ContinuationKind::Scheduler(_)))
    }

    pub fn has_resuming_effect(&self) -> bool {
        self.sites
            .iter()
            .any(|site| matches!(site.kind, ContinuationKind::ResumingEffect { .. }))
    }

    pub fn site(&self, block: BlockId, stmt_index: usize) -> Option<&ContinuationSite> {
        self.sites
            .iter()
            .find(|site| site.block == block && site.stmt_index == stmt_index)
    }
}

/// Analyze every scheduler suspension and statically resolved resuming effect
/// in a MIR function.
pub fn analyze(func: &mir::Function) -> ContinuationAnalysis {
    let live_after = live_after_statements(func);
    let mut sites = Vec::new();

    for block in &func.blocks {
        for (stmt_index, stmt) in block.stmts.iter().enumerate() {
            let Stmt::Assign { dst, op } = stmt else {
                continue;
            };
            let Some(kind) = continuation_kind(func, op) else {
                continue;
            };

            let resume_defs = assignment_defs(*dst, op);
            let resume_set: BTreeSet<LocalId> = resume_defs.iter().copied().collect();
            let mut live_across: Vec<LocalId> = live_after
                .get(&(block.id, stmt_index))
                .into_iter()
                .flat_map(|set| set.iter().copied())
                .filter(|local| !resume_set.contains(local))
                .collect();
            live_across.sort();
            live_across.dedup();

            sites.push(ContinuationSite {
                block: block.id,
                stmt_index,
                kind,
                resume_defs,
                live_across,
            });
        }
    }

    ContinuationAnalysis { sites }
}

/// Classify an rvalue as a continuation boundary.
///
/// Plain `ReceiveMatch` is intentionally absent: it is a non-blocking mailbox
/// scan in the core runtime. Timed `ReceiveWait` is the suspending form.
pub fn continuation_kind(func: &mir::Function, op: &RValue) -> Option<ContinuationKind> {
    if let RValue::Perform {
        resolved_handler: Some(href),
        ..
    } = op
    {
        if let Some(binding) = func
            .handler_tables
            .get(href.table_index as usize)
            .and_then(|table| table.bindings.get(href.binding_index as usize))
        {
            if binding.resume {
                return Some(ContinuationKind::ResumingEffect {
                    handler_body: binding.body,
                    single_shot: binding.single_shot,
                });
            }
            // A statically resolved abortive handler does not resume the
            // continuation and must not be reclassified as a host scheduler
            // suspension.
            return None;
        }
    }

    scheduler_suspend_kind(op).map(ContinuationKind::Scheduler)
}

/// Classify runtime scheduler suspension without requiring function context.
///
/// Backends that reject user-defined handlers before lowering (such as the
/// current WasmFX restricted profile) can use this directly.
pub fn scheduler_suspend_kind(op: &RValue) -> Option<SchedulerSuspendKind> {
    match op {
        RValue::Perform {
            effect, op, args, ..
        } if effect == "LLM" && op == "ask" && !args.is_empty() => {
            Some(SchedulerSuspendKind::LlmAsk)
        }
        RValue::PerformAsync { .. } => Some(SchedulerSuspendKind::AsyncEffect),
        RValue::SignalWait { .. } => Some(SchedulerSuspendKind::SignalWait),
        RValue::ReceiveWait { .. } => Some(SchedulerSuspendKind::ReceiveWait),
        _ => None,
    }
}

/// Return locals read by an rvalue.
pub fn rvalue_uses(op: &RValue) -> Vec<LocalId> {
    let mut out = Vec::new();
    match op {
        RValue::Const(_) | RValue::Panic(_) => {}
        RValue::Load(local) => out.push(*local),
        RValue::LoadFieldNamed { obj, .. } | RValue::LoadFieldPos { obj, .. } => out.push(*obj),
        RValue::ArrayLoad { arr, idx } => {
            out.push(*arr);
            out.push(*idx);
        }
        RValue::ArrayLen(arr) => out.push(*arr),
        RValue::ArrayLit(items) | RValue::Tuple(items) => out.extend_from_slice(items),
        RValue::Unary(_, local) => out.push(*local),
        RValue::Binary(_, lhs, rhs) | RValue::StringEq(lhs, rhs) | RValue::StrConcat(lhs, rhs) => {
            out.push(*lhs);
            out.push(*rhs);
        }
        RValue::Call { func, args } => {
            if let mir::FuncRef::Local(local) = func {
                out.push(*local);
            }
            out.extend_from_slice(args);
        }
        RValue::Closure { captures, .. } => out.extend_from_slice(captures),
        RValue::Record(fields) => out.extend(fields.iter().map(|(_, local)| *local)),
        RValue::RecordUpdate { base, overrides } => {
            out.push(*base);
            out.extend(overrides.iter().map(|(_, local)| *local));
        }
        RValue::Perform { args, .. }
        | RValue::PerformAsync { args, .. }
        | RValue::FFICall { args, .. } => out.extend_from_slice(args),
        RValue::SignalWait { .. }
        | RValue::Receive
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveCommit
        | RValue::SelfRef
        | RValue::StateGet { .. } => {}
        RValue::ReceiveWait { timeout, .. } => out.push(*timeout),
        RValue::Migrate { actor, node } => {
            out.push(*actor);
            out.push(*node);
        }
        RValue::CapabilityCheck { val } | RValue::Resume(val) => out.push(*val),
        RValue::Spawn {
            init, target_node, ..
        } => {
            if let Some(node) = target_node {
                out.push(*node);
            }
            for (_, init_op) in init {
                out.extend(rvalue_uses(init_op));
            }
        }
        RValue::Send { actor, args, .. } | RValue::Ask { actor, args, .. } => {
            out.push(*actor);
            out.extend_from_slice(args);
        }
    }
    out
}

/// Locals defined by an assignment, including implicit selective-receive
/// payload outputs.
pub fn assignment_defs(dst: LocalId, op: &RValue) -> Vec<LocalId> {
    let extra = match op {
        RValue::ReceiveMatch { max_params, .. } | RValue::ReceiveWait { max_params, .. } => {
            *max_params
        }
        _ => 0,
    };

    (0..=extra)
        .map(|offset| LocalId(dst.0 + offset as u32))
        .collect()
}

fn stmt_uses(stmt: &Stmt) -> Vec<LocalId> {
    match stmt {
        Stmt::Assign { op, .. } => rvalue_uses(op),
        Stmt::StoreFieldNamed { obj, src, .. } => vec![*obj, *src],
        Stmt::ArrayStore { arr, idx, src } => vec![*arr, *idx, *src],
        Stmt::Emit { args, .. } => args.clone(),
        Stmt::StateSet { src, .. } => vec![*src],
        Stmt::EnterHandle { .. } | Stmt::PopHandler | Stmt::ParallelMarker { .. } => Vec::new(),
    }
}

fn stmt_defs(stmt: &Stmt) -> Vec<LocalId> {
    match stmt {
        Stmt::Assign { dst, op } => assignment_defs(*dst, op),
        _ => Vec::new(),
    }
}

fn terminator_uses(term: &Terminator) -> Vec<LocalId> {
    match term {
        Terminator::Return(Some(local))
        | Terminator::Resume(local)
        | Terminator::Branch { cond: local, .. } => vec![*local],
        Terminator::Return(None) | Terminator::Jump(_) | Terminator::Unterminated => Vec::new(),
    }
}

fn normal_successors(func: &mir::Function) -> HashMap<BlockId, Vec<BlockId>> {
    func.blocks
        .iter()
        .map(|block| {
            let succs = match &block.terminator {
                Terminator::Jump(target) => vec![*target],
                Terminator::Branch { then_, else_, .. } => vec![*then_, *else_],
                _ => Vec::new(),
            };
            (block.id, succs)
        })
        .collect()
}

fn block_use_def(block: &mir::Block) -> (BTreeSet<LocalId>, BTreeSet<LocalId>) {
    let mut uses = BTreeSet::new();
    let mut defs = BTreeSet::new();

    for stmt in &block.stmts {
        for local in stmt_uses(stmt) {
            if !defs.contains(&local) {
                uses.insert(local);
            }
        }
        defs.extend(stmt_defs(stmt));
    }

    for local in terminator_uses(&block.terminator) {
        if !defs.contains(&local) {
            uses.insert(local);
        }
    }

    (uses, defs)
}

fn live_in_sets(func: &mir::Function) -> HashMap<BlockId, BTreeSet<LocalId>> {
    let succs = normal_successors(func);
    let mut use_sets = HashMap::new();
    let mut def_sets = HashMap::new();
    for block in &func.blocks {
        let (uses, defs) = block_use_def(block);
        use_sets.insert(block.id, uses);
        def_sets.insert(block.id, defs);
    }

    let mut live_in: HashMap<BlockId, BTreeSet<LocalId>> = HashMap::new();
    loop {
        let mut changed = false;
        for block in func.blocks.iter().rev() {
            let mut live_out = BTreeSet::new();
            if let Some(targets) = succs.get(&block.id) {
                for target in targets {
                    if let Some(target_live) = live_in.get(target) {
                        live_out.extend(target_live.iter().copied());
                    }
                }
            }

            let mut next = use_sets.get(&block.id).cloned().unwrap_or_default();
            let defs = def_sets.get(&block.id).cloned().unwrap_or_default();
            next.extend(live_out.difference(&defs).copied());

            if live_in.get(&block.id) != Some(&next) {
                live_in.insert(block.id, next);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    live_in
}

fn live_after_statements(func: &mir::Function) -> HashMap<(BlockId, usize), BTreeSet<LocalId>> {
    let succs = normal_successors(func);
    let live_in = live_in_sets(func);
    let mut result = HashMap::new();

    for block in &func.blocks {
        let mut live = BTreeSet::new();
        if let Some(targets) = succs.get(&block.id) {
            for target in targets {
                if let Some(target_live) = live_in.get(target) {
                    live.extend(target_live.iter().copied());
                }
            }
        }
        live.extend(terminator_uses(&block.terminator));

        for stmt_index in (0..block.stmts.len()).rev() {
            let stmt = &block.stmts[stmt_index];
            result.insert((block.id, stmt_index), live.clone());

            for def in stmt_defs(stmt) {
                live.remove(&def);
            }
            live.extend(stmt_uses(stmt));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{Block, Function, HandlerBindingDef, HandlerRef, HandlerTableDef};
    use crate::type_metadata::TypeMetadata;

    fn function(blocks: Vec<Block>, handler_tables: Vec<HandlerTableDef>) -> Function {
        Function {
            name: "test".into(),
            params: Vec::new(),
            captures: Vec::new(),
            ret: None,
            locals: Vec::new(),
            blocks,
            entry: BlockId(0),
            handler_tables,
            type_metadata: TypeMetadata::new(),
            line_table: Vec::new(),
            placement: None,
        }
    }

    #[test]
    fn receive_match_is_not_a_scheduler_suspension() {
        let func = function(
            vec![Block {
                id: BlockId(0),
                stmts: vec![Stmt::Assign {
                    dst: LocalId(0),
                    op: RValue::ReceiveMatch {
                        behavior_ids: vec![1],
                        max_params: 2,
                    },
                }],
                terminator: Terminator::Return(None),
            }],
            Vec::new(),
        );

        assert!(analyze(&func).sites.is_empty());
    }

    #[test]
    fn receive_wait_excludes_resume_defined_payload_slots_from_frame() {
        let func = function(
            vec![Block {
                id: BlockId(0),
                stmts: vec![
                    Stmt::Assign {
                        dst: LocalId(10),
                        op: RValue::ReceiveWait {
                            behavior_ids: vec![1],
                            max_params: 2,
                            timeout: LocalId(3),
                        },
                    },
                    Stmt::Assign {
                        dst: LocalId(20),
                        op: RValue::Load(LocalId(11)),
                    },
                    Stmt::Assign {
                        dst: LocalId(21),
                        op: RValue::Load(LocalId(7)),
                    },
                ],
                terminator: Terminator::Return(Some(LocalId(21))),
            }],
            Vec::new(),
        );

        let analysis = analyze(&func);
        let site = analysis.site(BlockId(0), 0).expect("receive wait site");
        assert_eq!(
            site.kind,
            ContinuationKind::Scheduler(SchedulerSuspendKind::ReceiveWait)
        );
        assert_eq!(
            site.resume_defs,
            vec![LocalId(10), LocalId(11), LocalId(12)]
        );
        assert_eq!(site.live_across, vec![LocalId(7)]);
    }

    #[test]
    fn resuming_effect_carries_single_shot_and_live_across_metadata() {
        let func = function(
            vec![
                Block {
                    id: BlockId(0),
                    stmts: vec![
                        Stmt::Assign {
                            dst: LocalId(4),
                            op: RValue::Perform {
                                effect: "State".into(),
                                op: "get".into(),
                                args: Vec::new(),
                                resolved_handler: Some(HandlerRef {
                                    table_index: 0,
                                    binding_index: 0,
                                }),
                            },
                        },
                        Stmt::Assign {
                            dst: LocalId(5),
                            op: RValue::Load(LocalId(9)),
                        },
                    ],
                    terminator: Terminator::Return(Some(LocalId(5))),
                },
                Block {
                    id: BlockId(1),
                    stmts: Vec::new(),
                    terminator: Terminator::Resume(LocalId(6)),
                },
            ],
            vec![HandlerTableDef {
                bindings: vec![HandlerBindingDef {
                    effect_name: "State".into(),
                    params: Vec::new(),
                    resume: true,
                    single_shot: true,
                    body: BlockId(1),
                }],
            }],
        );

        let analysis = analyze(&func);
        let site = analysis.site(BlockId(0), 0).expect("resuming effect site");
        assert_eq!(
            site.kind,
            ContinuationKind::ResumingEffect {
                handler_body: BlockId(1),
                single_shot: true,
            }
        );
        assert_eq!(site.resume_defs, vec![LocalId(4)]);
        assert_eq!(site.live_across, vec![LocalId(9)]);
    }

    #[test]
    fn cross_block_liveness_is_preserved_across_suspend() {
        let func = function(
            vec![
                Block {
                    id: BlockId(0),
                    stmts: vec![Stmt::Assign {
                        dst: LocalId(1),
                        op: RValue::SignalWait {
                            name: "ready".into(),
                        },
                    }],
                    terminator: Terminator::Jump(BlockId(1)),
                },
                Block {
                    id: BlockId(1),
                    stmts: vec![Stmt::Assign {
                        dst: LocalId(2),
                        op: RValue::Load(LocalId(8)),
                    }],
                    terminator: Terminator::Return(Some(LocalId(2))),
                },
            ],
            Vec::new(),
        );

        let analysis = analyze(&func);
        let site = analysis.site(BlockId(0), 0).expect("signal wait site");
        assert_eq!(site.live_across, vec![LocalId(8)]);
    }
}
