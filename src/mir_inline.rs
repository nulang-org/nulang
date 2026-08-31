//! MIR closure inlining pass.
//!
//! Identifies closures that are created and immediately called within the same
//! function (never stored, returned, sent, or captured by another closure) and
//! inlines their bodies at each call site, eliminating heap allocation and
//! indirect call overhead.
//!
//! The pass runs to a fixed point on the entire module — inlining one closure
//! may expose additional inlining opportunities.

use crate::mir::{Block, BlockId, FuncRef, Function, LocalId, Module, RValue, Stmt, Terminator};
use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Inline eligible local closures across all functions and behaviors in
/// `module`.  Returns the total number of call sites inlined.
pub fn inline_local_closures(module: &mut Module) -> u32 {
    let mut total = 0u32;

    // Safety cap: each round inlines at most one call site per block per
    // candidate.  In pathological cases (many closures, interacting blocks)
    // this could take many rounds, but it must terminate.  A hard limit
    // prevents infinite loops from logic bugs.
    const MAX_ROUNDS: u32 = 100;
    let mut round_count = 0u32;

    loop {
        round_count += 1;
        if round_count > MAX_ROUNDS {
            break;
        }

        let candidates = discover_candidates(&module.functions, &module.behaviors);
        if candidates.is_empty() {
            break;
        }

        let round = apply_candidates(&mut module.functions, &mut module.behaviors, candidates);
        if round == 0 {
            break;
        }
        total += round;
    }

    total
}

// ---------------------------------------------------------------------------
// Candidate discovery (immutable pass)
// ---------------------------------------------------------------------------

/// Opaque handle telling `apply_candidates` which container a caller lives in.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Container {
    Functions(usize),
    Behaviors(usize),
}

/// Snapshot of callee data needed for inlining, taken before the caller is
/// mutated.
struct CalleeSnapshot {
    blocks: Vec<Block>,
    locals: Vec<crate::mir::Local>,
    params: Vec<LocalId>,
    captures: Vec<LocalId>,
    /// Handler tables; non-empty means the callee uses effect handlers
    /// and cannot be inlined without remapping handler-table indices.
    has_handlers: bool,
    /// Whether the callee contains a self-referencing call (recursive
    /// closure).  These can't be inlined.
    is_recursive: bool,
}

/// Check whether a callee function cannot be inlined due to handlers or
/// self-recursion.
fn callee_properties(callee: &Function) -> (bool, bool) {
    let has_handlers = !callee.handler_tables.is_empty();

    let is_recursive = callee.blocks.iter().any(|block| {
        block.stmts.iter().any(|stmt| {
            matches!(
                stmt,
                Stmt::Assign {
                    op: RValue::Call { func: FuncRef::Local(l), .. },
                    ..
                } if !callee.params.contains(l) && !callee.captures.contains(l)
            )
        })
    });

    (has_handlers, is_recursive)
}

/// A fully-resolved inlining candidate with snapshotted callee data.
struct Candidate {
    container: Container,
    clos_local: LocalId,
    callee: CalleeSnapshot,
    captures: Vec<LocalId>,
    call_sites: Vec<CallSite>,
}

/// Scan all functions and behaviors for eligible closures.  Callee data is
/// cloned into snapshots so the mutable pass doesn't need to read from the
/// function list.
fn discover_candidates(funcs: &[Function], behaviors: &[Function]) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for i in 0..funcs.len() {
        for (clos_local, callee_idx, captures, call_sites) in find_eligible_closures(i, funcs) {
            let callee = match funcs.get(callee_idx) {
                Some(f) => f,
                None => continue,
            };
            candidates.push(Candidate {
                container: Container::Functions(i),
                clos_local,
                callee: {
                    let (hh, rec) = callee_properties(callee);
                    CalleeSnapshot {
                        blocks: callee.blocks.clone(),
                        locals: callee.locals.clone(),
                        params: callee.params.clone(),
                        captures: callee.captures.clone(),
                        has_handlers: hh,
                        is_recursive: rec,
                    }
                },
                captures,
                call_sites,
            });
        }
    }

    for i in 0..behaviors.len() {
        for (clos_local, callee_idx, captures, call_sites) in find_eligible_closures(i, behaviors) {
            // Closures always reference the function list, not the behavior list.
            let callee = match funcs.get(callee_idx) {
                Some(f) => f,
                None => continue,
            };
            candidates.push(Candidate {
                container: Container::Behaviors(i),
                clos_local,
                callee: {
                    let (hh, rec) = callee_properties(callee);
                    CalleeSnapshot {
                        blocks: callee.blocks.clone(),
                        locals: callee.locals.clone(),
                        params: callee.params.clone(),
                        captures: callee.captures.clone(),
                        has_handlers: hh,
                        is_recursive: rec,
                    }
                },
                captures,
                call_sites,
            });
        }
    }

    candidates
}

// ---------------------------------------------------------------------------
// Candidate application (mutable pass)
// ---------------------------------------------------------------------------

/// Apply inlining using the snapshotted candidates.  Only the last call site
/// per block is inlined per round; remaining sites are deferred to the next
/// fixed-point iteration.
fn apply_candidates(
    funcs: &mut Vec<Function>,
    behaviors: &mut Vec<Function>,
    mut candidates: Vec<Candidate>,
) -> u32 {
    let mut count = 0u32;

    // Track blocks modified by ANY candidate this round to prevent
    // stale call-site indices when two candidates reference the same block.
    let mut modified_blocks: FxHashMap<(Container, BlockId), ()> = FxHashMap::default();

    for c in &mut candidates {
        if c.call_sites.is_empty() {
            continue;
        }

        if c.callee.has_handlers || c.callee.is_recursive || c.callee.blocks.len() != 1 {
            continue;
        }

        // Sites in blocks already modified by another candidate this round
        // carry stale indices, so we cannot inline them now. Defer them to
        // `remaining` rather than discarding them: they are re-discovered
        // (with fresh indices) next round, and counting them in `remaining`
        // prevents the premature `remove_closure_allocation` that would
        // otherwise orphan still-referenced call sites into nil calls.
        let mut deferred = Vec::new();
        c.call_sites.retain(|site| {
            if modified_blocks.contains_key(&(c.container, site.block)) {
                deferred.push(CallSite {
                    block: site.block,
                    stmt_idx: site.stmt_idx,
                    dst: site.dst,
                    args: site.args.clone(),
                });
                false
            } else {
                true
            }
        });

        // Sort call sites by (block, stmt_idx) descending.
        c.call_sites
            .sort_by(|a, b| b.block.0.cmp(&a.block.0).then(b.stmt_idx.cmp(&a.stmt_idx)));

        // Inline at most one call site per block this round.
        let mut seen_blocks: FxHashMap<BlockId, ()> = FxHashMap::default();
        let mut inlined_this_round = Vec::new();
        let mut remaining = deferred;

        for site in &c.call_sites {
            if seen_blocks.contains_key(&site.block) {
                remaining.push(CallSite {
                    block: site.block,
                    stmt_idx: site.stmt_idx,
                    dst: site.dst,
                    args: site.args.clone(),
                });
            } else {
                seen_blocks.insert(site.block, ());
                inlined_this_round.push(site);
            }
        }

        if inlined_this_round.is_empty() {
            continue;
        }

        let caller: &mut Function = match c.container {
            Container::Functions(idx) => &mut funcs[idx],
            Container::Behaviors(idx) => &mut behaviors[idx],
        };

        for site in &inlined_this_round {
            inline_one_call(
                caller,
                site.block,
                site.stmt_idx,
                site.dst,
                &site.args,
                &c.callee.blocks,
                &c.callee.locals,
                &c.callee.params,
                &c.callee.captures,
                &c.captures,
            );
            modified_blocks.insert((c.container, site.block), ());
            count += 1;
        }

        if remaining.is_empty() {
            remove_closure_allocation(caller, c.clos_local);
        }
    }

    count
}

// ---------------------------------------------------------------------------
// Eligibility analysis
// ---------------------------------------------------------------------------

/// A discovered call site for an eligible closure.
struct CallSite {
    block: BlockId,
    /// Index of the `Stmt::Assign { .. RValue::Call { .. } }` within the block.
    stmt_idx: usize,
    /// The local receiving the call result.
    dst: LocalId,
    /// Arguments passed to the call.
    args: Vec<LocalId>,
}

/// Scan `funcs[caller_idx]` for closures eligible for inlining.  Returns
/// `(closure_local, callee_func_idx, captures, call_sites)` for each
/// eligible closure.
fn find_eligible_closures(
    caller_idx: usize,
    funcs: &[Function],
) -> Vec<(LocalId, usize, Vec<LocalId>, Vec<CallSite>)> {
    let caller = match funcs.get(caller_idx) {
        Some(f) => f,
        None => return Vec::new(),
    };
    let mut result = Vec::new();

    for (_block_idx, block) in caller.blocks.iter().enumerate() {
        for (_si, stmt) in block.stmts.iter().enumerate() {
            let (clos_local, callee_idx, captures) = match stmt {
                Stmt::Assign {
                    dst,
                    op: RValue::Closure { func, captures },
                } => (*dst, *func, captures.clone()),
                _ => continue,
            };

            let mut calls = Vec::new();
            if !all_uses_are_calls(caller, clos_local, &mut calls) {
                continue;
            }

            result.push((clos_local, callee_idx, captures, calls));
        }
    }

    result
}

/// Scan every statement and terminator in `func` for uses of `local`.  If
/// every use is a `Call { func: FuncRef::Local(local), .. }`, collect them
/// into `calls` and return `true`.  If any non-call use is found, return
/// `false` immediately (the closure escapes and cannot be inlined).
fn all_uses_are_calls(func: &Function, local: LocalId, calls: &mut Vec<CallSite>) -> bool {
    for (block_idx, block) in func.blocks.iter().enumerate() {
        let bid = BlockId(block_idx as u32);

        for (si, stmt) in block.stmts.iter().enumerate() {
            // Skip the closure allocation itself — `dst == local` on a
            // `Closure` RValue is the definition, not a use.
            if matches!(stmt, Stmt::Assign { dst, op: RValue::Closure { .. } } if *dst == local) {
                continue;
            }
            match stmt {
                Stmt::Assign {
                    dst,
                    op:
                        RValue::Call {
                            func: FuncRef::Local(f),
                            args,
                        },
                } if *f == local => {
                    calls.push(CallSite {
                        block: bid,
                        stmt_idx: si,
                        dst: *dst,
                        args: args.clone(),
                    });
                }
                _ => {
                    if stmt_uses_local(stmt, local) {
                        return false;
                    }
                }
            }
        }

        if terminator_uses_local(&block.terminator, local) {
            return false;
        }
    }
    true
}

/// Check whether `stmt` references `local` in any position.
fn stmt_uses_local(stmt: &Stmt, local: LocalId) -> bool {
    match stmt {
        Stmt::Assign { dst, op } => *dst == local || rvalue_uses_local(op, local),
        Stmt::StoreFieldNamed { obj, src, .. } => *obj == local || *src == local,
        Stmt::ArrayStore { arr, idx, src } => *arr == local || *idx == local || *src == local,
        Stmt::EnterHandle { .. } | Stmt::PopHandler | Stmt::StateSet { .. } | Stmt::Emit { .. } => {
            false
        }
    }
}

/// Check whether `rv` references `local` in any position.
fn rvalue_uses_local(rv: &RValue, local: LocalId) -> bool {
    match rv {
        RValue::Load(id) => *id == local,
        RValue::Panic(_) => false,
        RValue::Closure { func: _, captures } => captures.iter().any(|c| *c == local),
        RValue::Call { func, args } => {
            (match func {
                FuncRef::Local(id) => *id == local,
                FuncRef::Index(_) => false,
            }) || args.iter().any(|a| *a == local)
        }
        RValue::Tuple(vals) | RValue::ArrayLit(vals) => vals.iter().any(|v| *v == local),
        RValue::Record(fields) => fields.iter().any(|(_, v)| *v == local),
        RValue::RecordUpdate { base, overrides } => {
            *base == local || overrides.iter().any(|(_, v)| *v == local)
        }
        RValue::LoadFieldNamed { obj, .. } | RValue::LoadFieldPos { obj, .. } => *obj == local,
        RValue::ArrayLoad { arr, idx } | RValue::Binary(_, arr, idx) => {
            *arr == local || *idx == local
        }
        RValue::ArrayLen(id) | RValue::Unary(_, id) => *id == local,
        RValue::StringEq(a, b) => *a == local || *b == local,
        RValue::StrConcat(a, b) => *a == local || *b == local,
        RValue::Perform { args, .. } | RValue::PerformAsync { args, .. } => {
            args.iter().any(|a| *a == local)
        }
        RValue::SignalWait { .. } => false,
        RValue::Receive | RValue::ReceiveCommit => false,
        RValue::ReceiveMatch { .. } => false,
        RValue::ReceiveWait { timeout, .. } => *timeout == local,
        RValue::Spawn {
            init, target_node, ..
        } => {
            init.iter().any(|(_, rv)| rvalue_uses_local(rv, local))
                || target_node.map_or(false, |n| n == local)
        }
        RValue::Send { actor, args, .. } => *actor == local || args.iter().any(|a| *a == local),
        RValue::Ask { actor, args, .. } => *actor == local || args.iter().any(|a| *a == local),
        RValue::Resume(id) | RValue::CapabilityCheck { val: id } => *id == local,
        RValue::FFICall { args, .. } => args.iter().any(|a| *a == local),
        RValue::Migrate { actor, node } => *actor == local || *node == local,
        RValue::SelfRef | RValue::StateGet { .. } => false,
        RValue::Const(_) => false,
    }
}

/// Check whether `term` references `local`.
fn terminator_uses_local(term: &Terminator, local: LocalId) -> bool {
    match term {
        Terminator::Return(Some(id)) => *id == local,
        Terminator::Return(None) => false,
        Terminator::Jump(_) | Terminator::Unterminated => false,
        Terminator::Branch {
            cond,
            then_: _,
            else_: _,
        } => *cond == local,
        Terminator::Resume(id) => *id == local,
    }
}

// ---------------------------------------------------------------------------
// Inlining
// ---------------------------------------------------------------------------

/// Inline a single call site.  The callee body (from the snapshotted data) is
/// spliced into `caller`, replacing the call statement.
fn inline_one_call(
    caller: &mut Function,
    call_block: BlockId,
    call_stmt_idx: usize,
    call_dst: LocalId,
    call_args: &[LocalId],
    callee_blocks: &[Block],
    callee_locals: &[crate::mir::Local],
    callee_params: &[LocalId],
    callee_captures: &[LocalId],
    closure_captures: &[LocalId],
) {
    // Build the local-ID remapping: callee local → caller local.
    let mut remap: FxHashMap<LocalId, LocalId> = FxHashMap::default();

    // Map params to call args.
    for (param, arg) in callee_params.iter().zip(call_args.iter()) {
        remap.insert(*param, *arg);
    }

    // Map captures to closure capture values.
    for (cap, cap_val) in callee_captures.iter().zip(closure_captures.iter()) {
        remap.insert(*cap, *cap_val);
    }

    // Allocate fresh locals in the caller for remaining callee locals.
    let mut next_local = caller.locals.len() as u32;
    for local in callee_locals {
        if !remap.contains_key(&local.id) {
            let new_id = LocalId(next_local);
            next_local += 1;
            remap.insert(local.id, new_id);
            caller.locals.push(crate::mir::Local {
                id: new_id,
                name: local.name.clone(),
                ty: local.ty.clone(),
                cap: local.cap,
            });
        }
    }
    // Validate call-site index is still valid (block may have been modified
    // by a previous candidate's inlining despite our cross-candidate guard).
    let block_stmts = &caller.blocks[call_block.0 as usize].stmts;
    if call_stmt_idx >= block_stmts.len() {
        return;
    }
    // Verify the statement at stmt_idx is indeed a Call through our local.
    if !matches!(
        &block_stmts[call_stmt_idx],
        Stmt::Assign {
            op: RValue::Call {
                func: FuncRef::Local(_),
                ..
            },
            ..
        }
    ) {
        return;
    }

    // Split the caller's block at the call site.
    // Compute values that need `caller.blocks` BEFORE taking a mutable ref.
    let blocks_len = caller.blocks.len();
    let old_block_id = caller.blocks[call_block.0 as usize].id;
    let block = &mut caller.blocks[call_block.0 as usize];
    // Suffix: statements after the call (the call itself is at stmt_idx).
    let suffix_stmts: Vec<Stmt> = block.stmts.split_off(call_stmt_idx + 1);
    // Remove the call statement itself.
    block.stmts.pop();
    let suffix_terminator = std::mem::replace(&mut block.terminator, Terminator::Unterminated);

    // Build the inlined block from the callee's single block.
    let mut inlined_stmts: Vec<Stmt> = callee_blocks[0]
        .stmts
        .iter()
        .map(|s| remap_stmt(s, &remap))
        .collect();

    // The callee's Return becomes an assignment to the call destination.
    match &callee_blocks[0].terminator {
        Terminator::Return(Some(ret_val)) => {
            let ret = remap_local(*ret_val, &remap);
            inlined_stmts.push(Stmt::Assign {
                dst: call_dst,
                op: RValue::Load(ret),
            });
        }
        Terminator::Return(None) => {
            // Unit return — assign nil to the call destination.
            inlined_stmts.push(Stmt::Assign {
                dst: call_dst,
                op: RValue::Const(crate::bytecode::Constant::Int(0)),
            });
        }
        _ => {
            // Callee block doesn't end in Return — shouldn't happen for
            // well-formed closures, but if it does, just assign nil.
            inlined_stmts.push(Stmt::Assign {
                dst: call_dst,
                op: RValue::Const(crate::bytecode::Constant::Int(0)),
            });
        }
    }

    let inlined_bid = BlockId(blocks_len as u32);
    let suffix_bid = BlockId(blocks_len as u32 + 1);

    let old_line_table: Vec<_> = caller
        .line_table
        .iter()
        .filter(|((bid, _), _)| *bid == old_block_id)
        .cloned()
        .collect();

    // Wire prefix → inlined → suffix.
    block.terminator = Terminator::Jump(inlined_bid);

    caller.blocks.push(Block {
        id: inlined_bid,
        stmts: inlined_stmts,
        terminator: Terminator::Jump(suffix_bid),
    });

    caller.blocks.push(Block {
        id: suffix_bid,
        stmts: suffix_stmts,
        terminator: suffix_terminator,
    });

    // Update line table: prefix statements keep their old association;
    // suffix statements move to the new suffix block.
    caller
        .line_table
        .retain(|((bid, _), _)| *bid != old_block_id);
    for ((_, si), line) in &old_line_table {
        let si_usize = *si as usize;
        if si_usize < call_stmt_idx {
            caller.line_table.push(((old_block_id, *si), *line));
        } else if si_usize > call_stmt_idx {
            caller
                .line_table
                .push(((suffix_bid, si_usize - call_stmt_idx - 1), *line));
        }
        // The call statement itself (si == call_stmt_idx) is dropped.
    }
}

/// Remove the `Stmt::Assign { dst: clos, op: RValue::Closure { .. } }` that
/// allocated the now-dead closure.
fn remove_closure_allocation(func: &mut Function, clos: LocalId) {
    for block in &mut func.blocks {
        block.stmts.retain(|stmt| {
            !matches!(
                stmt,
                Stmt::Assign {
                    dst,
                    op: RValue::Closure { .. }
                } if *dst == clos
            )
        });
    }
}

// ---------------------------------------------------------------------------
// Remapping helpers
// ---------------------------------------------------------------------------

fn remap_local(id: LocalId, remap: &FxHashMap<LocalId, LocalId>) -> LocalId {
    remap.get(&id).copied().unwrap_or(id)
}

fn remap_locals(ids: &[LocalId], remap: &FxHashMap<LocalId, LocalId>) -> Vec<LocalId> {
    ids.iter().map(|id| remap_local(*id, remap)).collect()
}

fn remap_rvalue(rv: &RValue, remap: &FxHashMap<LocalId, LocalId>) -> RValue {
    match rv {
        RValue::Load(id) => RValue::Load(remap_local(*id, remap)),
        RValue::Panic(m) => RValue::Panic(m.clone()),
        RValue::Closure { func, captures } => RValue::Closure {
            func: *func,
            captures: remap_locals(captures, remap),
        },
        RValue::Call { func, args } => RValue::Call {
            func: remap_funcref(func, remap),
            args: remap_locals(args, remap),
        },
        RValue::Tuple(vals) => RValue::Tuple(remap_locals(vals, remap)),
        RValue::ArrayLit(vals) => RValue::ArrayLit(remap_locals(vals, remap)),
        RValue::Record(fields) => RValue::Record(
            fields
                .iter()
                .map(|(n, v)| (n.clone(), remap_local(*v, remap)))
                .collect(),
        ),
        RValue::RecordUpdate { base, overrides } => RValue::RecordUpdate {
            base: remap_local(*base, remap),
            overrides: overrides
                .iter()
                .map(|(n, v)| (n.clone(), remap_local(*v, remap)))
                .collect(),
        },
        RValue::LoadFieldNamed { obj, field } => RValue::LoadFieldNamed {
            obj: remap_local(*obj, remap),
            field: field.clone(),
        },
        RValue::LoadFieldPos { obj, index } => RValue::LoadFieldPos {
            obj: remap_local(*obj, remap),
            index: *index,
        },
        RValue::ArrayLoad { arr, idx } => RValue::ArrayLoad {
            arr: remap_local(*arr, remap),
            idx: remap_local(*idx, remap),
        },
        RValue::ArrayLen(id) => RValue::ArrayLen(remap_local(*id, remap)),
        RValue::Unary(op, id) => RValue::Unary(*op, remap_local(*id, remap)),
        RValue::Binary(op, a, b) => {
            RValue::Binary(*op, remap_local(*a, remap), remap_local(*b, remap))
        }
        RValue::StringEq(a, b) => RValue::StringEq(remap_local(*a, remap), remap_local(*b, remap)),
        RValue::StrConcat(a, b) => {
            RValue::StrConcat(remap_local(*a, remap), remap_local(*b, remap))
        }
        RValue::Perform {
            effect,
            op,
            args,
            resolved_handler,
        } => RValue::Perform {
            effect: effect.clone(),
            op: op.clone(),
            args: remap_locals(args, remap),
            resolved_handler: *resolved_handler,
        },
        RValue::PerformAsync {
            effect_op,
            args,
            resolved_handler,
        } => RValue::PerformAsync {
            effect_op: effect_op.clone(),
            args: remap_locals(args, remap),
            resolved_handler: *resolved_handler,
        },
        RValue::SignalWait { name } => RValue::SignalWait { name: name.clone() },
        RValue::Receive => RValue::Receive,
        RValue::ReceiveMatch {
            behavior_ids,
            max_params,
        } => RValue::ReceiveMatch {
            behavior_ids: behavior_ids.clone(),
            max_params: *max_params,
        },
        RValue::ReceiveWait {
            behavior_ids,
            max_params,
            timeout,
        } => RValue::ReceiveWait {
            behavior_ids: behavior_ids.clone(),
            max_params: *max_params,
            timeout: remap_local(*timeout, remap),
        },
        RValue::Spawn {
            behavior_idx,
            init,
            target_node,
            capabilities,
        } => RValue::Spawn {
            behavior_idx: *behavior_idx,
            init: init
                .iter()
                .map(|(n, rv)| (n.clone(), remap_rvalue(rv, remap)))
                .collect(),
            target_node: target_node.map(|n| remap_local(n, remap)),
            capabilities: capabilities.clone(),
        },
        RValue::Send {
            actor,
            behavior_idx,
            args,
            remote,
        } => RValue::Send {
            actor: remap_local(*actor, remap),
            behavior_idx: *behavior_idx,
            args: remap_locals(args, remap),
            remote: *remote,
        },
        RValue::Ask {
            actor,
            behavior_idx,
            args,
            remote,
            timeout_ms,
        } => RValue::Ask {
            actor: remap_local(*actor, remap),
            behavior_idx: *behavior_idx,
            args: remap_locals(args, remap),
            remote: *remote,
            timeout_ms: *timeout_ms,
        },
        RValue::Resume(id) => RValue::Resume(remap_local(*id, remap)),
        RValue::CapabilityCheck { val } => RValue::CapabilityCheck {
            val: remap_local(*val, remap),
        },
        RValue::FFICall { idx, args } => RValue::FFICall {
            idx: *idx,
            args: remap_locals(args, remap),
        },
        RValue::Migrate { actor, node } => RValue::Migrate {
            actor: remap_local(*actor, remap),
            node: remap_local(*node, remap),
        },
        RValue::SelfRef => RValue::SelfRef,
        RValue::StateGet { field } => RValue::StateGet {
            field: field.clone(),
        },
        RValue::ReceiveCommit => RValue::ReceiveCommit,
        RValue::Const(c) => RValue::Const(c.clone()),
    }
}

fn remap_funcref(fr: &FuncRef, remap: &FxHashMap<LocalId, LocalId>) -> FuncRef {
    match fr {
        FuncRef::Index(i) => FuncRef::Index(*i),
        FuncRef::Local(id) => FuncRef::Local(remap_local(*id, remap)),
    }
}

fn remap_stmt(stmt: &Stmt, remap: &FxHashMap<LocalId, LocalId>) -> Stmt {
    match stmt {
        Stmt::Assign { dst, op } => Stmt::Assign {
            dst: remap_local(*dst, remap),
            op: remap_rvalue(op, remap),
        },
        Stmt::StoreFieldNamed { obj, field, src } => Stmt::StoreFieldNamed {
            obj: remap_local(*obj, remap),
            field: field.clone(),
            src: remap_local(*src, remap),
        },
        Stmt::ArrayStore { arr, idx, src } => Stmt::ArrayStore {
            arr: remap_local(*arr, remap),
            idx: remap_local(*idx, remap),
            src: remap_local(*src, remap),
        },
        Stmt::EnterHandle { table } => Stmt::EnterHandle { table: *table },
        Stmt::PopHandler => Stmt::PopHandler,
        Stmt::Emit { event, args } => Stmt::Emit {
            event: event.clone(),
            args: remap_locals(args, remap),
        },
        Stmt::StateSet { field, src } => Stmt::StateSet {
            field: field.clone(),
            src: remap_local(*src, remap),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Constant;

    fn make_module() -> Module {
        Module::new("test")
    }

    fn add_function(m: &mut Module, _name: &str, f: Function) -> usize {
        m.functions.push(f);
        m.functions.len() - 1
    }

    /// Build a minimal MIR program: a caller that creates and immediately
    /// calls a single-expression closure.
    fn build_inlinable_program() -> Module {
        let mut m = make_module();

        // Closure function (callee): fn(x) -> x + x
        let mut callee = crate::mir::FunctionBuilder::new("__lambda_0", None);
        let p0 = callee.add_param("x", crate::types::Type::unit());
        let t0 = callee.add_temp(crate::types::Type::unit());
        callee.assign(t0, RValue::Binary(crate::ast::BinOp::Add, p0, p0));
        callee.terminate(Terminator::Return(Some(t0)));
        let callee_idx = add_function(&mut m, "__lambda_0", callee.build());

        // Caller: let c5 = 5 in let clos = fn(x) { x+x } in clos(c5)
        let mut caller = crate::mir::FunctionBuilder::new("caller", None);
        let c5 = caller.add_temp(crate::types::Type::unit());
        caller.assign(c5, RValue::Const(Constant::Int(5)));
        let clos = caller.add_temp(crate::types::Type::unit());
        caller.assign(
            clos,
            RValue::Closure {
                func: callee_idx,
                captures: vec![],
            },
        );
        let result = caller.add_temp(crate::types::Type::unit());
        caller.assign(
            result,
            RValue::Call {
                func: FuncRef::Local(clos),
                args: vec![c5],
            },
        );
        caller.terminate(Terminator::Return(Some(result)));
        add_function(&mut m, "caller", caller.build());

        m
    }

    #[test]
    fn test_find_eligible_empty() {
        let m = make_module();
        let result = find_eligible_closures(0, &m.functions);
        assert!(result.is_empty());
    }

    #[test]
    fn test_inline_simple_closure() {
        let mut m = build_inlinable_program();
        let count = inline_local_closures(&mut m);

        // One call site should have been inlined.
        assert_eq!(count, 1);

        // The caller should no longer have a Closure RValue.
        let caller = &m.functions[1];
        let has_closure = caller.blocks.iter().any(|b| {
            b.stmts.iter().any(|s| {
                matches!(
                    s,
                    Stmt::Assign {
                        op: RValue::Closure { .. },
                        ..
                    }
                )
            })
        });
        assert!(!has_closure, "closure allocation should be removed");

        // The caller should not have a Call through a local.
        let has_call = caller.blocks.iter().any(|b| {
            b.stmts.iter().any(|s| {
                matches!(
                    s,
                    Stmt::Assign {
                        op: RValue::Call {
                            func: FuncRef::Local(_),
                            ..
                        },
                        ..
                    }
                )
            })
        });
        assert!(!has_call, "call through local should be inlined");
    }

    #[test]
    fn test_no_inline_when_closure_stored() {
        let mut m = make_module();

        // Closure that returns its param.
        let mut callee = crate::mir::FunctionBuilder::new("__lambda_0", None);
        let p0 = callee.add_param("x", crate::types::Type::unit());
        callee.terminate(Terminator::Return(Some(p0)));
        let callee_idx = add_function(&mut m, "__lambda_0", callee.build());

        // Caller stores the closure in a record — should NOT be inlined.
        let mut caller = crate::mir::FunctionBuilder::new("caller", None);
        let clos = caller.add_temp(crate::types::Type::unit());
        caller.assign(
            clos,
            RValue::Closure {
                func: callee_idx,
                captures: vec![],
            },
        );
        let rec = caller.add_temp(crate::types::Type::unit());
        caller.assign(rec, RValue::Record(vec![("f".to_string(), clos)]));
        caller.terminate(Terminator::Return(Some(rec)));
        add_function(&mut m, "caller", caller.build());

        let count = inline_local_closures(&mut m);
        assert_eq!(count, 0, "closure stored in record should not be inlined");
    }

    #[test]
    fn test_no_inline_when_closure_returned() {
        let mut m = make_module();

        let mut callee = crate::mir::FunctionBuilder::new("__lambda_0", None);
        callee.terminate(Terminator::Return(None));
        let callee_idx = add_function(&mut m, "__lambda_0", callee.build());

        // Caller returns the closure — should NOT be inlined.
        let mut caller = crate::mir::FunctionBuilder::new("caller", None);
        let clos = caller.add_temp(crate::types::Type::unit());
        caller.assign(
            clos,
            RValue::Closure {
                func: callee_idx,
                captures: vec![],
            },
        );
        caller.terminate(Terminator::Return(Some(clos)));
        add_function(&mut m, "caller", caller.build());

        let count = inline_local_closures(&mut m);
        assert_eq!(count, 0, "returned closure should not be inlined");
    }

    #[test]
    fn test_inline_with_captures() {
        let mut m = make_module();

        // Closure: fn(x) -> x + a  (captures a)
        let mut callee = crate::mir::FunctionBuilder::new("__lambda_0", None);
        let px = callee.add_param("x", crate::types::Type::unit());
        let ca = callee.add_capture("a", crate::types::Type::unit());
        let t0 = callee.add_temp(crate::types::Type::unit());
        callee.assign(t0, RValue::Binary(crate::ast::BinOp::Add, px, ca));
        callee.terminate(Terminator::Return(Some(t0)));
        let callee_idx = add_function(&mut m, "__lambda_0", callee.build());

        // Caller: let a = 40 in let add = fn(x) { x + a } in add(2)
        let mut caller = crate::mir::FunctionBuilder::new("caller", None);
        let a = caller.add_temp(crate::types::Type::unit());
        caller.assign(a, RValue::Const(Constant::Int(40)));
        let c2 = caller.add_temp(crate::types::Type::unit());
        caller.assign(c2, RValue::Const(Constant::Int(2)));
        let clos = caller.add_temp(crate::types::Type::unit());
        caller.assign(
            clos,
            RValue::Closure {
                func: callee_idx,
                captures: vec![a],
            },
        );
        let result = caller.add_temp(crate::types::Type::unit());
        caller.assign(
            result,
            RValue::Call {
                func: FuncRef::Local(clos),
                args: vec![c2],
            },
        );
        caller.terminate(Terminator::Return(Some(result)));
        add_function(&mut m, "caller", caller.build());

        let count = inline_local_closures(&mut m);
        assert_eq!(count, 1, "closure with captures should be inlined");
    }

    #[test]
    fn test_idempotent() {
        let mut m = build_inlinable_program();
        let c1 = inline_local_closures(&mut m);
        let c2 = inline_local_closures(&mut m);
        assert_eq!(c1, 1);
        assert_eq!(c2, 0, "second pass should find nothing new");
    }
}
