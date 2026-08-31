//! AOT code generation: MIR → Cranelift CLIF.
//!
//! Compiles whole MIR functions to native code with unboxed parameter and
//! return types when type metadata is available. Falls back to NaN-tagged
//! runtime helpers when types are unknown.
//!
//! # Calling convention
//!
//! Compiled functions follow the C ABI:
//! ```c
//! uint64_t nulang_fn_N(uint64_t arg0, uint64_t arg1, ...);
//! ```
//! All arguments and return values are `u64` (NaN-tagged when type is
//! unknown, raw bits when unboxed). The AOT runtime trampoline handles
//! boxing/unboxing at function boundaries.

use cranelift::codegen::ir::{BlockArg, FuncRef};
use cranelift::prelude::*;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::JITModule;
use cranelift_module::{Linkage, Module};

use std::collections::{HashMap, HashSet};

use crate::mir;
use crate::type_metadata::{CapabilityMetadata, KnownType, TypeMetadata};

// Reuse NaN-tag constants and CLIF helpers from `cranelift_utils`.
use crate::cranelift_utils::{emit_sext48, emit_tag_bool, emit_tag_int, TAG_BOOL_I64, TAG_NIL_I64};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during AOT compilation.
#[derive(Debug)]
pub enum AotCompileError {
    /// A MIR construct that isn't yet supported by the AOT backend.
    Unsupported(String),
    /// Internal compiler error.
    Internal(String),
    /// Cranelift compilation failure.
    Cranelift(String),
}

impl std::fmt::Display for AotCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AotCompileError::Unsupported(msg) => write!(f, "AOT unsupported: {}", msg),
            AotCompileError::Internal(msg) => write!(f, "AOT internal error: {}", msg),
            AotCompileError::Cranelift(msg) => write!(f, "AOT cranelift error: {}", msg),
        }
    }
}

impl std::error::Error for AotCompileError {}

pub type AotResult<T> = Result<T, AotCompileError>;

// CLIF helpers imported from `cranelift_utils` (above).
// ---------------------------------------------------------------------------
// Compilation context
// ---------------------------------------------------------------------------

/// State maintained during compilation of one MIR module.
pub struct AotContext<'a> {
    /// The Cranelift JIT module.
    pub module: &'a mut JITModule,
    /// Reusable function builder context.
    pub builder_context: &'a mut FunctionBuilderContext,
    /// Cranelift codegen context (holds the current function being compiled).
    pub codegen_ctx: codegen::Context,
    /// Runtime helpers registered with the JIT module.
    pub helpers: HashMap<&'static str, FuncRef>,
    /// FuncIds of already-compiled functions, indexed by MIR function index.
    pub func_ids: Vec<cranelift_module::FuncId>,
    /// Capability metadata for each register.
    pub cap_metadata: CapabilityMetadata,
    /// Compilation mode: boxed (NaN-tagged) or unboxed (raw i64 for Int).
    pub mode: CompileMode,
    /// Module-wide field name → slot index mapping for records.
    pub field_map: HashMap<String, u8>,
    /// Module constant pool (for String constant resolution).
    pub constants: Vec<crate::bytecode::Constant>,
    /// Module foreign-function declarations, indexed by `RValue::FFICall.idx`.
    pub foreign_functions: Vec<mir::ForeignFunction>,
}
impl<'a> AotContext<'a> {
    pub fn new(module: &'a mut JITModule, builder_context: &'a mut FunctionBuilderContext) -> Self {
        let codegen_ctx = module.make_context();
        AotContext {
            module,
            builder_context,
            codegen_ctx,
            helpers: HashMap::new(),
            func_ids: Vec::new(),
            cap_metadata: CapabilityMetadata::new(),
            mode: CompileMode::Boxed,
            field_map: HashMap::new(),
            constants: Vec::new(),
            foreign_functions: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// SSA construction helpers
// ---------------------------------------------------------------------------

/// Edges from blocks containing a `perform` with a statically-resolved,
/// *resuming* handler to that handler's body block. These are not reflected
/// in any terminator — the handler body is entered via effect dispatch — but
/// the AOT backend compiles them as intra-function jumps (a resuming effect is
/// just control flow within the same native function). Without these edges the
/// handler body blocks would be unreachable in the successor graph and never
/// compiled.
fn effect_handler_edges(func: &mir::Function) -> Vec<(mir::BlockId, mir::BlockId)> {
    let mut edges = Vec::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign {
                op:
                    mir::RValue::Perform {
                        resolved_handler: Some(href),
                        ..
                    },
                ..
            } = stmt
            {
                if let Some(body) = func
                    .handler_tables
                    .get(href.table_index as usize)
                    .and_then(|t| t.bindings.get(href.binding_index as usize))
                    .map(|b| b.body)
                {
                    edges.push((block.id, body));
                }
            }
        }
    }
    edges
}

/// Compute block predecessors from terminators.
fn compute_predecessors(func: &mir::Function) -> HashMap<mir::BlockId, Vec<mir::BlockId>> {
    let mut preds: HashMap<mir::BlockId, Vec<mir::BlockId>> = HashMap::new();
    for block in &func.blocks {
        match &block.terminator {
            mir::Terminator::Jump(target) => {
                preds.entry(*target).or_default().push(block.id);
            }
            mir::Terminator::Branch { then_, else_, .. } => {
                preds.entry(*then_).or_default().push(block.id);
                preds.entry(*else_).or_default().push(block.id);
            }
            _ => {}
        }
    }
    for (src, dst) in effect_handler_edges(func) {
        // Multiple performs from the same block are the same predecessor;
        // dedup so live-in analysis doesn't over-approximate (which would
        // pull post-perform locals into a handler body's block params).
        let v = preds.entry(dst).or_default();
        if !v.contains(&src) {
            v.push(src);
        }
    }
    preds
}

/// Compute successors of each block for topological traversal.
fn compute_successors(func: &mir::Function) -> HashMap<mir::BlockId, Vec<mir::BlockId>> {
    let mut succs = compute_normal_successors(func);
    for (src, dst) in effect_handler_edges(func) {
        succs.entry(src).or_default().push(dst);
    }
    succs
}

/// Compute successors over NORMAL control flow only (Jump/Branch), excluding
/// the effect-handler edges. Used for continuation-liveness: a handler body is
/// not a normal flow successor, so its effect params must not count as live
/// into the perform block.
fn compute_normal_successors(func: &mir::Function) -> HashMap<mir::BlockId, Vec<mir::BlockId>> {
    let mut succs: HashMap<mir::BlockId, Vec<mir::BlockId>> = HashMap::new();
    for block in &func.blocks {
        let targets = match &block.terminator {
            mir::Terminator::Jump(target) => vec![*target],
            mir::Terminator::Branch { then_, else_, .. } => vec![*then_, *else_],
            _ => vec![],
        };
        succs.insert(block.id, targets);
    }
    succs
}

/// Compute reverse post-order (topological order) starting from the entry block.
fn reverse_postorder(func: &mir::Function) -> Vec<mir::BlockId> {
    let succs = compute_successors(func);
    let mut order = Vec::new();
    let mut visited = HashSet::new();
    // Recursive post-order DFS from entry.
    fn dfs(
        node: mir::BlockId,
        succs: &HashMap<mir::BlockId, Vec<mir::BlockId>>,
        visited: &mut HashSet<mir::BlockId>,
        order: &mut Vec<mir::BlockId>,
    ) {
        if !visited.insert(node) {
            return;
        }
        if let Some(children) = succs.get(&node) {
            for &child in children {
                dfs(child, succs, visited, order);
            }
        }
        order.push(node);
    }
    dfs(func.entry, &succs, &mut visited, &mut order);
    order.reverse();
    order
}

/// Count how many resuming `perform`s target each resuming handler body block.
/// When a resuming handler is invoked more than once, the handler body needs a
/// continuation-index block param so its `Terminator::Resume` can dispatch back
/// to the right perform site's continuation.
fn resuming_perform_count(func: &mir::Function) -> HashMap<mir::BlockId, usize> {
    let mut out: HashMap<mir::BlockId, usize> = HashMap::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign {
                op:
                    mir::RValue::Perform {
                        resolved_handler: Some(href),
                        ..
                    },
                ..
            } = stmt
            {
                if let Some(binding) = func
                    .handler_tables
                    .get(href.table_index as usize)
                    .and_then(|t| t.bindings.get(href.binding_index as usize))
                {
                    if binding.resume {
                        *out.entry(binding.body).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    out
}

/// A resuming `perform` site within a MIR function.
struct ResumingSite {
    /// The resuming handler body block.
    body: mir::BlockId,
    /// The block containing the perform.
    block: mir::BlockId,
    /// Statement index of the perform within `block`.
    idx: usize,
    /// The perform's destination register.
    dst: u32,
}

/// Enumerate every resuming `perform` site (those whose handler binding has
/// `resume == true`).
fn resuming_sites(func: &mir::Function) -> Vec<ResumingSite> {
    let local_base = mir::FunctionBuilder::LOCAL_BASE as u32;
    let mut out = Vec::new();
    for block in &func.blocks {
        for (idx, stmt) in block.stmts.iter().enumerate() {
            if let mir::Stmt::Assign {
                dst,
                op:
                    mir::RValue::Perform {
                        resolved_handler: Some(href),
                        ..
                    },
                ..
            } = stmt
            {
                if let Some(binding) = func
                    .handler_tables
                    .get(href.table_index as usize)
                    .and_then(|t| t.bindings.get(href.binding_index as usize))
                {
                    if binding.resume {
                        out.push(ResumingSite {
                            body: binding.body,
                            block: block.id,
                            idx,
                            dst: local_base + dst.0,
                        });
                    }
                }
            }
        }
    }
    out
}

/// For each resuming `perform` site (block, stmt index), the set of registers
/// live at the point the perform's continuation begins — i.e. the values the
/// post-perform code (or the block's successors) read that are NOT redefined
/// after the perform. Computed by a backward liveness walk per block starting
/// from each block's live-out set.
fn continuation_live_ins(
    func: &mir::Function,
    sites: &[ResumingSite],
) -> HashMap<(mir::BlockId, usize), HashSet<u32>> {
    let local_base = mir::FunctionBuilder::LOCAL_BASE as u32;
    // NORMAL successors only — the handler body is not a real flow successor,
    // so its effect params must not be treated as live into the perform block.
    let succs = compute_normal_successors(func);
    let live_ins = compute_live_ins(func, local_base, &succs);
    let live_out = |b: mir::BlockId| -> HashSet<u32> {
        let mut out = HashSet::new();
        if let Some(ss) = succs.get(&b) {
            for s in ss {
                if let Some(si) = live_ins.get(s) {
                    out.extend(si.iter().copied());
                }
            }
        }
        out
    };
    // Which (block, idx) are sites.
    let site_set: HashSet<(mir::BlockId, usize)> = sites.iter().map(|s| (s.block, s.idx)).collect();

    let mut out: HashMap<(mir::BlockId, usize), HashSet<u32>> = HashMap::new();
    for block in &func.blocks {
        let mut live = live_out(block.id);
        for i in (0..block.stmts.len()).rev() {
            let stmt = &block.stmts[i];
            let key = (block.id, i);
            if site_set.contains(&key) {
                // Record the continuation live-in (before this stmt's def).
                out.insert(key, live.clone());
            }
            // Backward transfer: live = (live - defs) ∪ uses.
            let mut defs: Vec<u32> = Vec::new();
            let mut uses: Vec<u32> = Vec::new();
            match stmt {
                mir::Stmt::Assign { dst, op } => {
                    defs.push(local_base + dst.0);
                    uses.extend(stmt_rvalue_uses(op).iter().map(|l| local_base + l.0));
                }
                mir::Stmt::StoreFieldNamed { obj, src, .. } => {
                    uses.push(local_base + obj.0);
                    uses.push(local_base + src.0);
                }
                mir::Stmt::ArrayStore { arr, idx, src } => {
                    uses.push(local_base + arr.0);
                    uses.push(local_base + idx.0);
                    uses.push(local_base + src.0);
                }
                mir::Stmt::StateSet { src, .. } => {
                    uses.push(local_base + src.0);
                }
                mir::Stmt::Emit { args, .. } => {
                    uses.extend(args.iter().map(|a| local_base + a.0));
                }
                _ => {}
            }
            for d in &defs {
                live.remove(d);
            }
            for u in uses {
                live.insert(u);
            }
        }
    }
    out
}

/// Threaded-slot analysis for multi-site resuming handlers. Returns:
/// - per-body uniform threaded width;
/// - per-site "extra" threaded values (the continuation live-ins, minus the
///   perform's own dst and the same-block prior results).
///
/// A resuming perform's continuation is a new CLIF block that is a successor
/// of the (possibly shared) handler body, so it is NOT dominated by the
/// perform's block and can only read what the handler's `Resume` dispatch
/// forwards to it. Besides the resume value (dst) and same-block prior
/// results, the continuation may read any value live at its entry — which can
/// include cross-block values (e.g. a mutable accumulator assigned in an
/// earlier block's perform and read after a later perform). Those must be
/// threaded through the handler too. The width is the max over sites of
/// (same-block priors + extras), and every site supplies its own set padded
/// to that width.
fn resuming_threading(
    func: &mir::Function,
) -> (
    HashMap<mir::BlockId, usize>,
    HashMap<(mir::BlockId, usize), Vec<u32>>,
) {
    let sites = resuming_sites(func);
    let cont_live = continuation_live_ins(func, &sites);
    // Same-block prior results per site: perform dsts earlier in the same block.
    let mut per_site_priors: HashMap<(mir::BlockId, usize), Vec<u32>> = HashMap::new();
    for site in &sites {
        let priors: Vec<u32> = sites
            .iter()
            .filter(|s| s.block == site.block && s.idx < site.idx)
            .map(|s| s.dst)
            .collect();
        per_site_priors.insert((site.block, site.idx), priors);
    }
    // Extras per site = continuation live-ins minus dst minus same-block priors.
    let mut site_extras: HashMap<(mir::BlockId, usize), Vec<u32>> = HashMap::new();
    for site in &sites {
        let key = (site.block, site.idx);
        let priors: HashSet<u32> = per_site_priors
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut extras: Vec<u32> = cont_live
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|&r| r != site.dst && !priors.contains(&r))
            .collect();
        extras.sort_unstable();
        site_extras.insert(key, extras);
    }
    // Width per body = max over sites of (priors.len() + extras.len()).
    let mut width: HashMap<mir::BlockId, usize> = HashMap::new();
    for site in &sites {
        let key = (site.block, site.idx);
        let n = per_site_priors.get(&key).map(|p| p.len()).unwrap_or(0)
            + site_extras.get(&key).map(|e| e.len()).unwrap_or(0);
        width
            .entry(site.body)
            .and_modify(|x| *x = (*x).max(n))
            .or_insert(n);
    }
    (width, site_extras)
}

/// Collect the MIR locals a statement's RValue reads (as registers).
fn stmt_rvalue_uses(op: &mir::RValue) -> Vec<mir::LocalId> {
    let mut out = Vec::new();
    match op {
        mir::RValue::Load(l) => out.push(*l),
        mir::RValue::Panic(_) => {}
        mir::RValue::LoadFieldNamed { obj, .. } => out.push(*obj),
        mir::RValue::LoadFieldPos { obj, .. } => out.push(*obj),
        mir::RValue::ArrayLoad { arr, idx } => {
            out.push(*arr);
            out.push(*idx);
        }
        mir::RValue::ArrayLen(a) => out.push(*a),
        mir::RValue::ArrayLit(items) => out.extend_from_slice(items),
        mir::RValue::Unary(_, l) => out.push(*l),
        mir::RValue::Binary(_, a, b) => {
            out.push(*a);
            out.push(*b);
        }
        mir::RValue::StringEq(a, b) | mir::RValue::StrConcat(a, b) => {
            out.push(*a);
            out.push(*b);
        }
        mir::RValue::Call { args, .. }
        | mir::RValue::FFICall { args, .. }
        | mir::RValue::PerformAsync { args, .. } => out.extend_from_slice(args),
        mir::RValue::Perform { args, .. } => out.extend_from_slice(args),
        mir::RValue::Closure { captures, .. } => out.extend_from_slice(captures),
        mir::RValue::Tuple(items) => out.extend_from_slice(items),
        mir::RValue::Record(fields) => {
            for (_, v) in fields {
                out.push(*v);
            }
        }
        mir::RValue::RecordUpdate { base, overrides } => {
            out.push(*base);
            for (_, v) in overrides {
                out.push(*v);
            }
        }
        mir::RValue::SignalWait { .. }
        | mir::RValue::Receive
        | mir::RValue::ReceiveMatch { .. }
        | mir::RValue::ReceiveCommit
        | mir::RValue::SelfRef
        | mir::RValue::StateGet { .. } => {}
        mir::RValue::ReceiveWait { timeout, .. } => out.push(*timeout),
        mir::RValue::Migrate { actor, node } => {
            out.push(*actor);
            out.push(*node);
        }
        mir::RValue::CapabilityCheck { val } => out.push(*val),
        mir::RValue::Spawn {
            init, target_node, ..
        } => {
            if let Some(n) = target_node {
                out.push(*n);
            }
            for (_, rv) in init {
                out.extend(stmt_rvalue_uses(rv));
            }
        }
        mir::RValue::Send { actor, args, .. } | mir::RValue::Ask { actor, args, .. } => {
            out.push(*actor);
            out.extend_from_slice(args);
        }
        mir::RValue::Resume(l) => out.push(*l),
        mir::RValue::Const(_) => {}
    }
    out
}

/// Per-block live-in sets (registers) over the normal + handler CFG, used by
/// the cross-block resuming-perform guard. A register is live-in to a block if
/// it may be read on some path from that block's entry before being redefined.
fn compute_live_ins(
    func: &mir::Function,
    local_base: u32,
    succs: &HashMap<mir::BlockId, Vec<mir::BlockId>>,
) -> HashMap<mir::BlockId, HashSet<u32>> {
    // `gen[block]` = locals used before their first definition in the block
    // (a value used only AFTER being defined in the same block is not live-in).
    // `kill[block]` = locals defined in the block. Backward fixpoint:
    //   live_in(block) = gen ∪ (live_out − kill),  live_out = ∪ live_in(succ).
    let mut gen: HashMap<mir::BlockId, HashSet<u32>> = HashMap::new();
    let mut kill: HashMap<mir::BlockId, HashSet<u32>> = HashMap::new();
    for block in &func.blocks {
        let mut g = HashSet::new();
        let mut k = HashSet::new();
        // Assign statements: uses first (gen, unless already defined this
        // block), then the def (kill).
        let stmt_uses = |g: &mut HashSet<u32>, k: &HashSet<u32>, op: &mir::RValue| {
            for l in stmt_rvalue_uses(op) {
                let reg = local_base + l.0;
                if !k.contains(&reg) {
                    g.insert(reg);
                }
            }
        };
        for stmt in &block.stmts {
            match stmt {
                mir::Stmt::Assign { dst, op } => {
                    stmt_uses(&mut g, &k, op);
                    k.insert(local_base + dst.0);
                }
                mir::Stmt::StoreFieldNamed { obj, src, .. } => {
                    for l in [*obj, *src] {
                        let reg = local_base + l.0;
                        if !k.contains(&reg) {
                            g.insert(reg);
                        }
                    }
                }
                mir::Stmt::ArrayStore { arr, idx, src } => {
                    for l in [*arr, *idx, *src] {
                        let reg = local_base + l.0;
                        if !k.contains(&reg) {
                            g.insert(reg);
                        }
                    }
                }
                mir::Stmt::StateSet { src, .. } => {
                    let reg = local_base + src.0;
                    if !k.contains(&reg) {
                        g.insert(reg);
                    }
                }
                mir::Stmt::Emit { args, .. } => {
                    for a in args {
                        let reg = local_base + a.0;
                        if !k.contains(&reg) {
                            g.insert(reg);
                        }
                    }
                }
                _ => {}
            }
        }
        // Terminator uses (after all defs). A terminator operand that was
        // defined earlier in this block is NOT a live-in (gen) — it is killed.
        let term_uses = |g: &mut HashSet<u32>, k: &HashSet<u32>, id: mir::LocalId| {
            let reg = local_base + id.0;
            if !k.contains(&reg) {
                g.insert(reg);
            }
        };
        match &block.terminator {
            mir::Terminator::Return(Some(l)) => term_uses(&mut g, &k, *l),
            mir::Terminator::Branch { cond, .. } => term_uses(&mut g, &k, *cond),
            mir::Terminator::Resume(id) => term_uses(&mut g, &k, *id),
            _ => {}
        }
        gen.insert(block.id, g);
        kill.insert(block.id, k);
    }

    let mut live_in: HashMap<mir::BlockId, HashSet<u32>> = HashMap::new();
    loop {
        let mut changed = false;
        for block in &func.blocks {
            let mut out: HashSet<u32> = HashSet::new();
            if let Some(ss) = succs.get(&block.id) {
                for s in ss {
                    if let Some(si) = live_in.get(s) {
                        out.extend(si.iter().copied());
                    }
                }
            }
            let g = gen.get(&block.id).cloned().unwrap_or_default();
            let k = kill.get(&block.id).cloned().unwrap_or_default();
            let mut inn = g;
            for v in out {
                if !k.contains(&v) {
                    inn.insert(v);
                }
            }
            if live_in.get(&block.id).map(|s| s != &inn).unwrap_or(true) {
                live_in.insert(block.id, inn);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    live_in
}

/// Map each effect-handler body block (resuming OR abortive) to its declared
/// effect parameters (MIR locals). A `perform` passes its args into these as
/// block params, so the handler body can read them like a callee reads
/// parameters.
fn effect_handler_body_params(func: &mir::Function) -> HashMap<mir::BlockId, Vec<mir::LocalId>> {
    let mut out: HashMap<mir::BlockId, Vec<mir::LocalId>> = HashMap::new();
    for table in &func.handler_tables {
        for binding in &table.bindings {
            out.insert(binding.body, binding.params.clone());
        }
    }
    out
}

/// For each block, collect the set of register indices that are:
/// - Last assigned in at least one predecessor, AND
/// - The block has >1 predecessor.
///
/// These locals need CLIF block parameters for proper SSA merging.
///
/// Effect-handler body blocks are EXCLUDED: they are reached by `perform`
/// jumps (not normal control-flow merges), read only their declared effect
/// params (already block params) plus outer locals that flow through the
/// shared `local_vals` (dominance-scoped), and must not inherit a perform
/// block's post-perform locals — most importantly the perform result
/// destination, which flows BACK through the continuation, not into the
/// handler body.
fn compute_liveins(
    func: &mir::Function,
    preds: &HashMap<mir::BlockId, Vec<mir::BlockId>>,
    local_base: u32,
    handler_body_params: &HashMap<mir::BlockId, Vec<mir::LocalId>>,
) -> HashMap<mir::BlockId, Vec<u32>> {
    // First, for each block, find which locals are last-assigned in that block.
    let mut block_defs: HashMap<mir::BlockId, HashSet<u32>> = HashMap::new();
    for block in &func.blocks {
        let mut defs = HashSet::new();
        for stmt in &block.stmts {
            if let mir::Stmt::Assign { dst, .. } = stmt {
                defs.insert(local_base + dst.0);
            }
        }
        block_defs.insert(block.id, defs);
    }

    // Live-in sets (proper gen/kill liveness) for phi placement.
    let full_succs = compute_successors(func);
    let live_ins = compute_live_ins(func, local_base, &full_succs);

    // For each block with >1 predecessor, compute locals that need a CLIF block
    // param so the merge can read the right SSA value on every incoming path.
    let mut liveins: HashMap<mir::BlockId, Vec<u32>> = HashMap::new();
    for block in &func.blocks {
        if handler_body_params.contains_key(&block.id) {
            continue;
        }
        let pids = match preds.get(&block.id) {
            Some(p) if p.len() > 1 => p,
            _ => continue,
        };
        // (1) Locals defined in ALL predecessors (the historical heuristic;
        //     kept as a safe superset — dead members are harmless).
        let mut merged: HashSet<u32> = block_defs.get(&pids[0]).cloned().unwrap_or_default();
        for pid in &pids[1..] {
            if let Some(defs) = block_defs.get(pid) {
                merged = merged.intersection(defs).copied().collect();
            } else {
                merged.clear();
                break;
            }
        }
        // (2) Locals live into the merge AND defined in at least one predecessor
        //     get DIFFERENT reaching definitions from different paths (a branch
        //     assigns the variable on one path, another path carries the prior
        //     value), so they must be merged as a block param. Without this, a
        //     value assigned in one branch and read after the join is referenced
        //     from a non-dominating block → CLIF verifier error.
        if let Some(li) = live_ins.get(&block.id) {
            for &v in li {
                let defined_in_pred = pids
                    .iter()
                    .any(|pid| block_defs.get(pid).map(|d| d.contains(&v)).unwrap_or(false));
                if defined_in_pred {
                    merged.insert(v);
                }
            }
        }
        if !merged.is_empty() {
            let mut sorted: Vec<u32> = merged.into_iter().collect();
            sorted.sort();
            liveins.insert(block.id, sorted);
        }
    }
    liveins
}

/// Like `compile_terminator` but passes block-param values for merged locals.
fn compile_terminator_with_params(
    builder: &mut FunctionBuilder,
    term: &mir::Terminator,
    block_map: &HashMap<mir::BlockId, cranelift::prelude::Block>,
    block_params: &HashMap<mir::BlockId, Vec<u32>>,
    local_vals: &HashMap<u32, Value>,
    _mode: CompileMode,
    current_block: mir::BlockId,
    handler_continuations: &HashMap<mir::BlockId, Vec<(cranelift::prelude::Block, u32)>>,
) -> AotResult<()> {
    match term {
        mir::Terminator::Return(val) => {
            if let Some(id) = val {
                let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
                let v = *local_vals.get(&reg).ok_or_else(|| {
                    AotCompileError::Internal("return value uninitialized".into())
                })?;
                builder.ins().return_(&[v]);
            } else {
                let nil = builder
                    .ins()
                    .iconst(types::I64, 0x7FF8_0000_0000_0000u64 as i64);
                builder.ins().return_(&[nil]);
            }
            Ok(())
        }
        mir::Terminator::Jump(target) => {
            let clif_block = *block_map
                .get(target)
                .ok_or_else(|| AotCompileError::Internal("jump to unknown block".into()))?;
            let args = block_param_args(block_params, target, local_vals);
            builder.ins().jump(clif_block, &args);
            Ok(())
        }
        mir::Terminator::Branch { cond, then_, else_ } => {
            let cond_reg = mir::FunctionBuilder::LOCAL_BASE + cond.0;
            let cond_val = *local_vals
                .get(&cond_reg)
                .ok_or_else(|| AotCompileError::Internal("branch cond uninitialized".into()))?;
            let then_block = *block_map
                .get(then_)
                .ok_or_else(|| AotCompileError::Internal("branch then unknown".into()))?;
            let else_block = *block_map
                .get(else_)
                .ok_or_else(|| AotCompileError::Internal("branch else unknown".into()))?;

            let false_val = builder.ins().iconst(types::I64, TAG_BOOL_I64);
            let is_true = builder.ins().icmp(IntCC::NotEqual, cond_val, false_val);
            let then_args = block_param_args(block_params, then_, local_vals);
            let else_args = block_param_args(block_params, else_, local_vals);
            builder
                .ins()
                .brif(is_true, then_block, &then_args, else_block, &else_args);
            Ok(())
        }
        mir::Terminator::Resume(id) => {
            // Resuming handler body: restore the continuation by jumping back
            // to the block that follows the originating `perform`, passing the
            // resume value into the perform's destination local.
            let conts = handler_continuations.get(&current_block).ok_or_else(|| {
                AotCompileError::Internal(
                    "Terminator::Resume in a block with no captured continuation".into(),
                )
            })?;
            let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
            let v = *local_vals
                .get(&reg)
                .ok_or_else(|| AotCompileError::Internal("resume value uninitialized".into()))?;
            if conts.len() == 1 {
                // Single continuation: pass the resume value plus any threaded
                // slots the continuation carries (continuation live-ins / prior
                // results). No continuation-index param exists for a single
                // site, so threaded slots start at block_params.len().
                let idx_pos = block_params
                    .get(&current_block)
                    .map(|p| p.len())
                    .unwrap_or(0);
                let cur = builder
                    .current_block()
                    .ok_or_else(|| AotCompileError::Internal("Resume outside a block".into()))?;
                let hparams = builder.block_params(cur);
                let mut args: Vec<BlockArg> = vec![BlockArg::from(v)];
                args.extend(hparams[idx_pos..].iter().copied().map(BlockArg::from));
                builder.ins().jump(conts[0].0, &args);
                return Ok(());
            }
            // Multiple perform sites share this handler body. Dispatch on the
            // continuation-index block param (position = block_params.len())
            // to the matching continuation. Each continuation receives the
            // resume value plus the FULL uniform-width threaded slot set. The
            // values present are those supplied by whichever perform site
            // entered this invocation; each continuation binds only the
            // same-block prior slots it actually has, so the excess (cross-block
            // or padded) slots are ignored by all continuations.
            let idx_pos = block_params
                .get(&current_block)
                .map(|p| p.len())
                .unwrap_or(0);
            let cur = builder
                .current_block()
                .ok_or_else(|| AotCompileError::Internal("Resume outside a block".into()))?;
            let hparams = builder.block_params(cur);
            let mut idx_val = hparams[idx_pos];
            let mut resume_val = v;
            // Threaded prior perform results, ordered as in the perform sites.
            let mut threaded: Vec<Value> = hparams[idx_pos + 1..].to_vec();
            for (i, (cont, _dst)) in conts.iter().enumerate() {
                let idx_const = builder.ins().iconst(types::I64, i as i64);
                let eq = builder.ins().icmp(IntCC::Equal, idx_val, idx_const);
                let mut args: Vec<BlockArg> = vec![BlockArg::from(resume_val)];
                args.extend(threaded.iter().copied().map(BlockArg::from));
                if i == conts.len() - 1 {
                    // Last case: unconditional.
                    builder.ins().jump(*cont, &args);
                } else {
                    // Fall-through chain. The next link can't read this block's
                    // SSA values, so carry the dispatch index, resume value,
                    // and threaded results as the next block's params.
                    let next = builder.create_block();
                    for _ in 0..(2 + threaded.len()) {
                        builder.append_block_param(next, types::I64);
                    }
                    let mut else_args: Vec<BlockArg> =
                        vec![BlockArg::from(idx_val), BlockArg::from(resume_val)];
                    else_args.extend(threaded.iter().copied().map(BlockArg::from));
                    builder.ins().brif(eq, *cont, &args, next, &else_args);
                    builder.switch_to_block(next);
                    let nparams = builder.block_params(next);
                    idx_val = nparams[0];
                    resume_val = nparams[1];
                    threaded = nparams[2..].to_vec();
                }
            }
            Ok(())
        }
        mir::Terminator::Unterminated => Err(AotCompileError::Internal(
            "reached Unterminated terminator in codegen — this is a compiler bug".into(),
        )),
    }
}

/// Build the argument list for a jump/branch to `target`: one Value per
/// block parameter, taken from the current `local_vals`.
fn block_param_args(
    block_params: &HashMap<mir::BlockId, Vec<u32>>,
    target: &mir::BlockId,
    local_vals: &HashMap<u32, Value>,
) -> Vec<BlockArg> {
    if let Some(params) = block_params.get(target) {
        params
            .iter()
            .map(|reg| {
                let val = *local_vals.get(reg).expect("block param local missing");
                BlockArg::from(val)
            })
            .collect()
    } else {
        vec![]
    }
}
// ---------------------------------------------------------------------------
// Main entry point: compile a MIR function to a native function pointer
// ---------------------------------------------------------------------------

/// Whether to emit NaN-tagged (boxed) or raw (unboxed) integer values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompileMode {
    /// NaN-tagged i64 — the default, interoperable representation.
    Boxed,
    /// Raw i64 for Int types — faster, but requires type knowledge at call sites.
    Unboxed,
}

/// Check whether a function is eligible for unboxed compilation:
/// all params are `KnownType::Int` and the return type is Int or void.
pub fn is_all_int(func: &mir::Function) -> bool {
    // Functions with handled effects (resuming or abortive) must stay boxed.
    // The perform-result and handler-param locals carry Unknown type metadata,
    // so operations on them fall back to the NaN-tagged runtime helpers
    // (`nulang_iadd` etc.) — which misread raw (unboxed) operands as floats.
    // Boxed operands are properly tagged, so the helpers compute correctly.
    if !effect_handler_edges(func).is_empty() {
        return false;
    }
    // FFICall crosses into the runtime with boxed argument values; an unboxed
    // (raw) Int argument would be misread by the FFI marshaller. Functions
    // that call foreign functions must stay boxed.
    for block in &func.blocks {
        for stmt in &block.stmts {
            if matches!(
                stmt,
                mir::Stmt::Assign {
                    op: mir::RValue::FFICall { .. },
                    ..
                }
            ) {
                return false;
            }
        }
    }
    // PerformAsync routes boxed argument values to the runtime's async-effect
    // dispatcher; an unboxed (raw) Int argument would be misread there.
    for block in &func.blocks {
        for stmt in &block.stmts {
            if matches!(
                stmt,
                mir::Stmt::Assign {
                    op: mir::RValue::PerformAsync { .. },
                    ..
                }
            ) {
                return false;
            }
        }
    }
    // SignalWait delivers a boxed signal value (unit/nil); an unboxed function
    // would misread it if used as a raw int.
    for block in &func.blocks {
        for stmt in &block.stmts {
            if matches!(
                stmt,
                mir::Stmt::Assign {
                    op: mir::RValue::SignalWait { .. },
                    ..
                }
            ) {
                return false;
            }
        }
    }
    // Migrate delivers a boxed unit value; an unboxed function would misread
    // it if used as a raw int.
    for block in &func.blocks {
        for stmt in &block.stmts {
            if matches!(
                stmt,
                mir::Stmt::Assign {
                    op: mir::RValue::Migrate { .. },
                    ..
                }
            ) {
                return false;
            }
        }
    }
    // Nil-producing / heap-object operations must stay boxed. An unboxed
    // function tags its raw result, so a nil (div-by-zero, negative int-pow
    // exponent, out-of-bounds array access) would be re-tagged as int 0, and
    // heap objects hold tagged values the unboxed raw int path would corrupt.
    for block in &func.blocks {
        for stmt in &block.stmts {
            let nil_or_object = match stmt {
                mir::Stmt::Assign { op, .. } => matches!(
                    op,
                    mir::RValue::Binary(
                        crate::ast::BinOp::Div | crate::ast::BinOp::Mod | crate::ast::BinOp::Pow,
                        ..
                    ) | mir::RValue::ArrayLit(_)
                        | mir::RValue::ArrayLoad { .. }
                        | mir::RValue::ArrayLen(_)
                        | mir::RValue::Record(_)
                        | mir::RValue::Tuple(_)
                        | mir::RValue::RecordUpdate { .. }
                        | mir::RValue::LoadFieldNamed { .. }
                        | mir::RValue::LoadFieldPos { .. }
                ),
                mir::Stmt::StoreFieldNamed { .. } | mir::Stmt::ArrayStore { .. } => true,
                _ => false,
            };
            if nil_or_object {
                return false;
            }
        }
    }
    // Captured closures allocate a closure object holding boxed capture values
    // and dispatch through a runtime helper; the capture slots must be tagged.
    for block in &func.blocks {
        for stmt in &block.stmts {
            if matches!(
                stmt,
                mir::Stmt::Assign {
                    op: mir::RValue::Closure { captures, .. },
                    ..
                } if !captures.is_empty()
            ) {
                return false;
            }
        }
    }
    // Lifted closure functions receive captured values as trailing boxed
    // params; an unboxed variant would misread them as raw ints.
    if !func.captures.is_empty() {
        return false;
    }
    // A call through a closure value whose target is not statically known
    // (a parameter, a recursive-closure const binding, or a captured closure)
    // dispatches through the runtime helper with boxed arguments; an unboxed
    // caller would pass raw Ints the helper misreads. Only direct calls to
    // statically-known uncaptured closures stay unboxed.
    let mut direct_closure_locals: HashSet<u32> = HashSet::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign {
                dst,
                op: mir::RValue::Closure { captures, .. },
                ..
            } = stmt
            {
                if captures.is_empty() {
                    direct_closure_locals.insert(mir::FunctionBuilder::LOCAL_BASE + dst.0);
                }
            }
        }
    }
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign {
                op:
                    mir::RValue::Call {
                        func: mir::FuncRef::Local(id),
                        ..
                    },
                ..
            } = stmt
            {
                let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
                if !direct_closure_locals.contains(&reg) {
                    return false;
                }
            }
        }
    }
    let local_base = mir::FunctionBuilder::LOCAL_BASE as usize;
    for param in &func.params {
        let reg = local_base + param.0 as usize;
        if func.type_metadata.get_type(reg) != KnownType::Int {
            return false;
        }
    }
    // Return type: None (unit) is fine, Some(Int) is fine, anything else disqualifies.
    if let Some(ref ret_ty) = func.ret {
        match ret_ty {
            crate::types::Type::Primitive(crate::types::PrimitiveType::Int) => {}
            _ => return false,
        }
    }
    true
}

/// Compile the body of a MIR function that was already declared.
///
pub fn compile_mir_function_body(
    aot: &mut AotContext,
    mir_func: &mir::Function,
    _func_index: usize,
    func_id: cranelift_module::FuncId,
    mode: CompileMode,
) -> AotResult<()> {
    aot.mode = mode;
    // Reconstruct the signature for the codegen context. Lifted closure
    // functions receive their captured values as trailing params (in capture
    // order), appended after the explicit params.
    let mut sig = aot.module.make_signature();
    for _ in &mir_func.params {
        sig.params.push(AbiParam::new(types::I64));
    }
    for _ in &mir_func.captures {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.returns.push(AbiParam::new(types::I64));
    aot.codegen_ctx.func.signature = sig;
    // Split module and codegen_ctx for independent borrows.
    // Extract refs to constants and field_map before the split.
    let constants: &[crate::bytecode::Constant] = &aot.constants;
    let field_map: &HashMap<String, u8> = &aot.field_map;
    let module: &mut JITModule = aot.module;
    let codegen_ctx: &mut codegen::Context = &mut aot.codegen_ctx;
    let builder_ctx: &mut FunctionBuilderContext = aot.builder_context;
    let local_base = mir::FunctionBuilder::LOCAL_BASE;
    let type_meta = mir_func.type_metadata.clone();
    aot.cap_metadata = CapabilityMetadata::from_mir_function(mir_func);

    // Analyze block predecessors.
    let preds = compute_predecessors(mir_func);

    // Map each effect-handler body block to its declared effect parameters.
    // Used both to give handler bodies their effect-param block params and to
    // exclude them from merge-live-in analysis.
    let handler_body_params = effect_handler_body_params(mir_func);

    // For each block, collect locals assigned in any predecessor that are
    // used in this block — these need block params when multiple preds exist.
    let block_liveins = compute_liveins(mir_func, &preds, local_base, &handler_body_params);

    // Pre-resolve cross-function call targets (will fill inside builder scope).
    let mut call_targets: HashMap<usize, FuncRef> = HashMap::new();

    let _helpers = {
        let mut builder = FunctionBuilder::new(&mut codegen_ctx.func, builder_ctx);
        let entry_block = builder.create_block();
        builder.switch_to_block(entry_block);
        builder.append_block_params_for_function_params(entry_block);

        // Register runtime helpers with proper signatures.
        let mut h: HashMap<&str, FuncRef> = HashMap::new();

        // Binary helpers: (i64, i64) -> i64
        let bin_helpers: &[&str] = &[
            "nulang_iadd",
            "nulang_isub",
            "nulang_imul",
            "nulang_idiv",
            "nulang_imod",
            "nulang_icmp_eq",
            "nulang_icmp_lt",
            "nulang_icmp_gt",
            "nulang_icmp_le",
            "nulang_icmp_ge",
            "nulang_fadd",
            "nulang_fsub",
            "nulang_fmul",
            "nulang_fdiv",
            "nulang_fcmp_eq",
            "nulang_fcmp_lt",
            "nulang_fcmp_gt",
            "nulang_and",
            "nulang_or",
            "nulang_xor",
            "nulang_shl",
            "nulang_shr",
            "nulang_bitand",
            "nulang_bitor",
        ];
        for name in bin_helpers {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(*name, func_ref);
        }

        // Unary helpers: (i64) -> i64
        let unary_helpers: &[&str] = &[
            "nulang_ineg",
            "nulang_iinc",
            "nulang_idec",
            "nulang_not",
            "nulang_itof",
            "nulang_ftoi",
            "nulang_fneg",
        ];
        for name in unary_helpers {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(*name, func_ref);
        }

        // Add new AOT helpers (bin: pow, str_eq, str_concat, obj_get)
        let extra_bin: &[&str] = &[
            "nulang_pow",
            "nulang_str_eq",
            "nulang_str_concat",
            "nulang_obj_get",
        ];
        for name in extra_bin {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(*name, func_ref);
        }

        // Add unary helpers for obj_len, rec_copy
        let extra_unary: &[&str] = &["nulang_obj_len", "nulang_rec_copy"];
        for name in extra_unary {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(*name, func_ref);
        }

        // alloc_obj: (i64, i32) -> i64
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.params.push(AbiParam::new(types::I32));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_alloc_obj", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_alloc_obj", func_ref);
        }

        // obj_set: (i64, i64, i64) -> void
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.params.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_obj_set", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_obj_set", func_ref);
        }
        // --- AOT actor runtime helpers ---
        // self_ref: () -> i64
        {
            let mut h_sig = module.make_signature();
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_self_ref", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_self_ref", func_ref);
        }
        // state_get: (i64) -> i64
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_state_get", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_state_get", func_ref);
        }
        // state_set: (i64, i64) -> void
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.params.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_state_set", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_state_set", func_ref);
        }
        // send helpers: nulang_aot_send_N(target, behavior, arg0..argN-1) -> void
        const AOT_SEND_HELPERS: [&str; 9] = [
            "nulang_aot_send_0",
            "nulang_aot_send_1",
            "nulang_aot_send_2",
            "nulang_aot_send_3",
            "nulang_aot_send_4",
            "nulang_aot_send_5",
            "nulang_aot_send_6",
            "nulang_aot_send_7",
            "nulang_aot_send_8",
        ];
        for (n, h_name) in AOT_SEND_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // target actor ref
            h_sig.params.push(AbiParam::new(types::I64)); // behavior index
            for _ in 0..n {
                h_sig.params.push(AbiParam::new(types::I64));
            }
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        // perform helpers: nulang_aot_perform_N(eff, op, arg0..argN-1) -> i64
        const AOT_PERFORM_HELPERS: [&str; 9] = [
            "nulang_aot_perform_0",
            "nulang_aot_perform_1",
            "nulang_aot_perform_2",
            "nulang_aot_perform_3",
            "nulang_aot_perform_4",
            "nulang_aot_perform_5",
            "nulang_aot_perform_6",
            "nulang_aot_perform_7",
            "nulang_aot_perform_8",
        ];
        for (n, h_name) in AOT_PERFORM_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // effect name const
            h_sig.params.push(AbiParam::new(types::I64)); // op name const
            for _ in 0..n {
                h_sig.params.push(AbiParam::new(types::I64));
            }
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        // emit helpers: nulang_aot_emit_N(event, arg0..argN-1) -> void
        const AOT_EMIT_HELPERS: [&str; 9] = [
            "nulang_aot_emit_0",
            "nulang_aot_emit_1",
            "nulang_aot_emit_2",
            "nulang_aot_emit_3",
            "nulang_aot_emit_4",
            "nulang_aot_emit_5",
            "nulang_aot_emit_6",
            "nulang_aot_emit_7",
            "nulang_aot_emit_8",
        ];
        for (n, h_name) in AOT_EMIT_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // event name const
            for _ in 0..n {
                h_sig.params.push(AbiParam::new(types::I64));
            }
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        // ask helpers: nulang_aot_ask_N(actor, behavior, arg0..argN-1) -> i64
        const AOT_ASK_HELPERS: [&str; 9] = [
            "nulang_aot_ask_0",
            "nulang_aot_ask_1",
            "nulang_aot_ask_2",
            "nulang_aot_ask_3",
            "nulang_aot_ask_4",
            "nulang_aot_ask_5",
            "nulang_aot_ask_6",
            "nulang_aot_ask_7",
            "nulang_aot_ask_8",
        ];
        for (n, h_name) in AOT_ASK_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // target actor ref
            h_sig.params.push(AbiParam::new(types::I64)); // behavior index
            for _ in 0..n {
                h_sig.params.push(AbiParam::new(types::I64));
            }
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        // ffi helpers: nulang_aot_ffi_call_N(lib, sym, sig, arg0..argN-1) -> i64
        const AOT_FFI_CALL_HELPERS: [&str; 5] = [
            "nulang_aot_ffi_call_0",
            "nulang_aot_ffi_call_1",
            "nulang_aot_ffi_call_2",
            "nulang_aot_ffi_call_3",
            "nulang_aot_ffi_call_4",
        ];
        for (n, h_name) in AOT_FFI_CALL_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // library (TAG_STRING)
            h_sig.params.push(AbiParam::new(types::I64)); // symbol (TAG_STRING)
            h_sig.params.push(AbiParam::new(types::I64)); // bit-packed signature
            for _ in 0..n {
                h_sig.params.push(AbiParam::new(types::I64));
            }
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        // closure helpers: nulang_aot_make_closure_N(fn_idx, cap0..capN-1)
        // -> i64 and nulang_aot_call_closure_N(closure, arg0..argN-1) -> i64
        const AOT_MAKE_CLOSURE_HELPERS: [&str; 9] = [
            "nulang_aot_make_closure_0",
            "nulang_aot_make_closure_1",
            "nulang_aot_make_closure_2",
            "nulang_aot_make_closure_3",
            "nulang_aot_make_closure_4",
            "nulang_aot_make_closure_5",
            "nulang_aot_make_closure_6",
            "nulang_aot_make_closure_7",
            "nulang_aot_make_closure_8",
        ];
        for (n, h_name) in AOT_MAKE_CLOSURE_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // fn index
            for _ in 0..n {
                h_sig.params.push(AbiParam::new(types::I64)); // captures
            }
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        const AOT_CALL_CLOSURE_HELPERS: [&str; 9] = [
            "nulang_aot_call_closure_0",
            "nulang_aot_call_closure_1",
            "nulang_aot_call_closure_2",
            "nulang_aot_call_closure_3",
            "nulang_aot_call_closure_4",
            "nulang_aot_call_closure_5",
            "nulang_aot_call_closure_6",
            "nulang_aot_call_closure_7",
            "nulang_aot_call_closure_8",
        ];
        for (n, h_name) in AOT_CALL_CLOSURE_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // closure value
            for _ in 0..n {
                h_sig.params.push(AbiParam::new(types::I64)); // explicit args
            }
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        // async-effect helpers: nulang_aot_perform_async_N(effect, arg0..)
        // -> i64
        const AOT_PERFORM_ASYNC_HELPERS: [&str; 9] = [
            "nulang_aot_perform_async_0",
            "nulang_aot_perform_async_1",
            "nulang_aot_perform_async_2",
            "nulang_aot_perform_async_3",
            "nulang_aot_perform_async_4",
            "nulang_aot_perform_async_5",
            "nulang_aot_perform_async_6",
            "nulang_aot_perform_async_7",
            "nulang_aot_perform_async_8",
        ];
        for (n, h_name) in AOT_PERFORM_ASYNC_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // effect name (TAG_STRING)
            for _ in 0..n {
                h_sig.params.push(AbiParam::new(types::I64));
            }
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        // signal-wait helper: nulang_aot_signal_wait(name) -> i64
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64)); // signal name (TAG_STRING)
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_signal_wait", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_signal_wait", func_ref);
        }
        // migrate helper: nulang_aot_migrate(actor, node) -> i64
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_migrate", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_migrate", func_ref);
        }
        // receive helpers: nulang_aot_receive_match_N(id0..idN-1) -> i64,
        // nulang_aot_receive_payload(i) -> i64
        const AOT_RECEIVE_HELPERS: [&str; 8] = [
            "nulang_aot_receive_match_1",
            "nulang_aot_receive_match_2",
            "nulang_aot_receive_match_3",
            "nulang_aot_receive_match_4",
            "nulang_aot_receive_match_5",
            "nulang_aot_receive_match_6",
            "nulang_aot_receive_match_7",
            "nulang_aot_receive_match_8",
        ];
        for (n, h_name) in AOT_RECEIVE_HELPERS.iter().enumerate() {
            let mut h_sig = module.make_signature();
            for _ in 0..=n {
                h_sig.params.push(AbiParam::new(types::I64));
            }
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function(h_name, Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert(h_name, func_ref);
        }
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_receive_payload", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_receive_payload", func_ref);
        }
        // receive_pop: () -> i64 (pop-any mailbox receive)
        {
            let mut h_sig = module.make_signature();
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_receive_pop", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_receive_pop", func_ref);
        }
        // spawn_push: (i64, i64) -> () (queue one spawn init pair)
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.params.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_spawn_push", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_spawn_push", func_ref);
        }
        // spawn: (i64) -> i64 (create a standalone actor)
        {
            let mut h_sig = module.make_signature();
            h_sig.params.push(AbiParam::new(types::I64));
            h_sig.returns.push(AbiParam::new(types::I64));
            let h_id = module
                .declare_function("nulang_aot_spawn", Linkage::Import, &h_sig)
                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;
            let func_ref = module.declare_func_in_func(h_id, builder.func);
            h.insert("nulang_aot_spawn", func_ref);
        }
        // Helper to register a call target FuncRef.
        let mut register_call_target = |n: usize| {
            if !call_targets.contains_key(&n) {
                if let Some(&callee_fid) = aot.func_ids.get(n) {
                    let local_ref = module.declare_func_in_func(callee_fid, builder.func);
                    call_targets.insert(n, local_ref);
                }
            }
        };

        // Pre-scan: register all call targets from Call and Closure rvalues.
        for block in &mir_func.blocks {
            for stmt in &block.stmts {
                match stmt {
                    mir::Stmt::Assign {
                        op:
                            mir::RValue::Call {
                                func: mir::FuncRef::Index(n),
                                ..
                            },
                        ..
                    } => {
                        register_call_target(*n);
                    }
                    mir::Stmt::Assign {
                        op: mir::RValue::Closure { func, captures },
                        ..
                    } if captures.is_empty() => {
                        register_call_target(*func);
                    }
                    _ => {}
                }
            }
        }

        // Track which locals hold zero-capture closures for call resolution,
        // and which hold captured closures (those calls dispatch through the
        // runtime closure helper with the stored capture values).
        let mut closure_targets: HashMap<u32, usize> = HashMap::new();
        let mut captured_closure_locals: HashSet<u32> = HashSet::new();
        let mut local_vals: HashMap<u32, Value> = HashMap::new();

        for (i, param_id) in mir_func.params.iter().enumerate() {
            let reg = local_base + param_id.0;
            let val = builder.block_params(entry_block)[i];
            local_vals.insert(reg, val);
        }
        for (i, cap_id) in mir_func.captures.iter().enumerate() {
            let reg = local_base + cap_id.0;
            let val = builder.block_params(entry_block)[mir_func.params.len() + i];
            local_vals.insert(reg, val);
        }

        // Resuming handler body blocks receive their effect parameters as
        // block params (a `perform` jumps to them with the arg values).
        // (`handler_body_params` is computed above, before `compute_liveins`.)
        // A resuming handler invoked more than once needs a continuation-index
        // block param (appended last) so its `Terminator::Resume` can dispatch
        // to the right perform site's continuation.
        let resuming_counts = resuming_perform_count(mir_func);
        // Resuming handler bodies invoked from >= 2 perform sites: their
        // `Terminator::Resume` must dispatch on a continuation-index param.
        let multi_cont_bodies: HashSet<mir::BlockId> = resuming_counts
            .iter()
            .filter(|(_, &c)| c >= 2)
            .map(|(&b, _)| b)
            .collect();
        // Uniform threaded-slot width per resuming handler body: the max
        // Uniform threaded width per resuming handler body plus the per-site
        // extra continuation live-ins that must thread through the handler.
        // Every perform site supplies (same-block priors + its extras) padded
        // to the width, so the handler body's block-param count is consistent
        // across sites whether they share one block or live on exclusive
        // cross-block paths — and a continuation can read cross-block values
        // (e.g. a mutable accumulator set by an earlier block's perform).
        let (handler_threaded_width, site_extras) = resuming_threading(mir_func);

        // Create CLIF blocks — allocate block params for merge blocks.
        let mut block_map: HashMap<mir::BlockId, cranelift::prelude::Block> = HashMap::new();
        // Track which locals have block params in each block.
        let mut block_params: HashMap<mir::BlockId, Vec<u32>> = HashMap::new();
        for block in &mir_func.blocks {
            let clif_block = if block.id == mir_func.entry {
                entry_block
            } else {
                let blk = builder.create_block();
                // Effect-handler body (resuming or abortive): force block
                // params for its declared effect parameters
                // (local_base + LocalId), in declared order.
                if let Some(params) = handler_body_params.get(&block.id) {
                    let mut regs = Vec::with_capacity(params.len());
                    for p in params {
                        let reg = local_base + p.0;
                        builder.append_block_param(blk, types::I64);
                        regs.push(reg);
                    }
                    if !regs.is_empty() {
                        block_params.insert(block.id, regs);
                    }
                }
                // Add block params for locals that need merging.
                if let Some(liveins) = block_liveins.get(&block.id) {
                    let mut params = Vec::new();
                    for &reg in liveins {
                        builder.append_block_param(blk, types::I64);
                        params.push(reg);
                    }
                    // Merge with handler-effect params (dedup, stable order:
                    // effect params first, then live-ins).
                    let mut all: Vec<u32> =
                        block_params.get(&block.id).cloned().unwrap_or_default();
                    for reg in params {
                        if !all.contains(&reg) {
                            all.push(reg);
                        }
                    }
                    if !all.is_empty() {
                        block_params.insert(block.id, all);
                    }
                }
                // A resuming handler body gets a continuation-index param when
                // invoked from multiple perform sites, and the threaded slots
                // (continuation live-ins + prior results) whenever any site has
                // them (width > 0) — even a single site whose continuation
                // reads a live value (e.g. a loop-carried accumulator). These
                // are not local regs, so kept out of `block_params`; cont_idx
                // sits at CLIF position `block_params.len()`, threaded after.
                let width = handler_threaded_width.get(&block.id).copied().unwrap_or(0);
                if resuming_counts.get(&block.id).copied().unwrap_or(0) >= 2 {
                    builder.append_block_param(blk, types::I64);
                }
                for _ in 0..width {
                    builder.append_block_param(blk, types::I64);
                }
                blk
            };
            block_map.insert(block.id, clif_block);
        }

        // Compute topological block order so that each block's predecessors
        // are compiled before it, ensuring local_vals is populated.
        let block_order = reverse_postorder(mir_func);

        // Debug: dump MIR when verbose.
        if std::env::var("NULANG_DUMP_MIR").is_ok() {
            eprintln!(
                "=== AOT compiling fn_{} ({}) ===",
                _func_index, mir_func.name
            );
            for &bid in &block_order {
                let block = &mir_func.blocks[bid.0 as usize];
                eprintln!("  Block[{}] (preds: {:?}):", bid.0, preds.get(&bid));
                for stmt in &block.stmts {
                    eprintln!("    {:?}", stmt);
                }
                eprintln!("    term: {:?}", block.terminator);
            }
            eprintln!("  block_params: {:?}", block_params);
        }

        // Continuation map: resuming handler body block -> the continuations
        // created by each `perform` site (in perform order). Populated when a
        // resuming `perform` is compiled; read by the handler body's
        // `Terminator::Resume`. More than one entry means Resume dispatches on
        // the continuation-index block param.
        let mut handler_continuations: HashMap<
            mir::BlockId,
            Vec<(cranelift::prelude::Block, u32)>,
        > = HashMap::new();

        // Compile blocks in topological order.
        for &bid in &block_order {
            let block = &mir_func.blocks[bid.0 as usize];
            let clif_block = block_map[&bid];
            builder.switch_to_block(clif_block);

            // Read block parameters into local_vals for non-entry blocks.
            if block.id != mir_func.entry {
                if let Some(params) = block_params.get(&block.id) {
                    for (i, &reg) in params.iter().enumerate() {
                        let val = builder.block_params(clif_block)[i];
                        local_vals.insert(reg, val);
                    }
                }
            }

            // Per-block continuation thread: destination regs of prior
            // resuming performs in this block, threaded into later
            // continuations so they can read earlier perform results.
            let mut cont_thread: Vec<u32> = Vec::new();
            for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
                compile_stmt(
                    &mut builder,
                    stmt,
                    &type_meta,
                    &h,
                    &call_targets,
                    &mut closure_targets,
                    &mut captured_closure_locals,
                    &mut local_vals,
                    mode,
                    constants,
                    field_map,
                    &aot.foreign_functions,
                    &mir_func.handler_tables,
                    &block_map,
                    &block_params,
                    &mut handler_continuations,
                    &multi_cont_bodies,
                    &handler_threaded_width,
                    &site_extras,
                    &mut cont_thread,
                    stmt_idx,
                    bid,
                )?;
            }
            compile_terminator_with_params(
                &mut builder,
                &block.terminator,
                &block_map,
                &block_params,
                &local_vals,
                mode,
                block.id,
                &handler_continuations,
            )?;
        }

        builder.seal_all_blocks();
        builder.finalize();
        h
    };
    // Debug: dump CLIF when verbose.
    if std::env::var("NULANG_DUMP_CLIF").is_ok() {
        eprintln!("=== CLIF for fn_{} ===", _func_index);
        eprintln!("{}", codegen_ctx.func.display());
    }

    module
        .define_function(func_id, codegen_ctx)
        .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;

    module.clear_context(codegen_ctx);

    Ok(())
}

/// Generate a thin boxing wrapper for an all-Int function.
///
/// The wrapper takes tagged i64 arguments, untags them, calls the unboxed
/// variant, tags the result, and returns. This replaces the boxed body so
/// that callers always go through the wrapper — the original boxed body
/// is never compiled.
pub fn compile_boxing_wrapper(
    aot: &mut AotContext,
    param_count: usize,
    boxed_fid: cranelift_module::FuncId,
    unboxed_fid: cranelift_module::FuncId,
) -> AotResult<()> {
    // Split module and codegen_ctx for independent borrows.
    let module: &mut JITModule = aot.module;
    let codegen_ctx: &mut codegen::Context = &mut aot.codegen_ctx;
    let builder_ctx: &mut FunctionBuilderContext = aot.builder_context;
    // Set up function signature: tagged i64 params, tagged i64 return.
    let mut sig = module.make_signature();
    for _ in 0..param_count {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.returns.push(AbiParam::new(types::I64));
    codegen_ctx.func.signature = sig;

    let mut builder = FunctionBuilder::new(&mut codegen_ctx.func, builder_ctx);
    let entry_block = builder.create_block();
    builder.switch_to_block(entry_block);
    builder.append_block_params_for_function_params(entry_block);

    // Get unboxed function reference.
    let callee_ref = module.declare_func_in_func(unboxed_fid, builder.func);

    // Untag each parameter.
    let params: Vec<Value> = builder.block_params(entry_block).to_vec();
    let unboxed_args: Vec<Value> = params
        .iter()
        .map(|&p| emit_sext48(&mut builder, p))
        .collect();

    // Call unboxed variant.
    let call = builder.ins().call(callee_ref, &unboxed_args);
    let raw_result = builder.inst_results(call)[0];

    // Tag result and return.
    let tagged = emit_tag_int(&mut builder, raw_result);
    builder.ins().return_(&[tagged]);

    builder.seal_all_blocks();
    builder.finalize();

    // Debug: dump CLIF when verbose.
    if std::env::var("NULANG_DUMP_CLIF").is_ok() {
        eprintln!("=== CLIF for boxing wrapper ({}) ===", param_count);
        eprintln!("{}", codegen_ctx.func.display());
    }

    module
        .define_function(boxed_fid, codegen_ctx)
        .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;

    module.clear_context(codegen_ctx);

    Ok(())
}

// ---------------------------------------------------------------------------
// Statement compilation
// ---------------------------------------------------------------------------

fn compile_stmt(
    builder: &mut FunctionBuilder,
    stmt: &mir::Stmt,
    type_meta: &TypeMetadata,
    helpers: &HashMap<&str, FuncRef>,
    call_targets: &HashMap<usize, FuncRef>,
    closure_targets: &mut HashMap<u32, usize>,
    captured_closure_locals: &mut HashSet<u32>,
    local_vals: &mut HashMap<u32, Value>,
    mode: CompileMode,
    constants: &[crate::bytecode::Constant],
    field_map: &HashMap<String, u8>,
    foreign_functions: &[mir::ForeignFunction],
    handler_tables: &[mir::HandlerTableDef],
    block_map: &HashMap<mir::BlockId, cranelift::prelude::Block>,
    block_params: &HashMap<mir::BlockId, Vec<u32>>,
    handler_continuations: &mut HashMap<mir::BlockId, Vec<(cranelift::prelude::Block, u32)>>,
    multi_cont_bodies: &HashSet<mir::BlockId>,
    handler_threaded_width: &HashMap<mir::BlockId, usize>,
    site_extras: &HashMap<(mir::BlockId, usize), Vec<u32>>,
    cont_thread: &mut Vec<u32>,
    stmt_idx: usize,
    current_block: mir::BlockId,
) -> AotResult<()> {
    match stmt {
        mir::Stmt::Assign {
            dst,
            op: mir::RValue::ReceiveCommit,
        } => {
            // No-op in AOT: the standalone receive_match already removed the
            // matched message from the mailbox.
            let dst_reg = mir::FunctionBuilder::LOCAL_BASE + dst.0;
            local_vals.insert(dst_reg, builder.ins().iconst(types::I64, 0));
            Ok(())
        }
        mir::Stmt::Assign {
            dst,
            op:
                mir::RValue::ReceiveMatch {
                    behavior_ids,
                    max_params,
                }
                | mir::RValue::ReceiveWait {
                    behavior_ids,
                    max_params,
                    ..
                },
        } => {
            let helper_name = match behavior_ids.len() {
                1 => "nulang_aot_receive_match_1",
                2 => "nulang_aot_receive_match_2",
                3 => "nulang_aot_receive_match_3",
                4 => "nulang_aot_receive_match_4",
                5 => "nulang_aot_receive_match_5",
                6 => "nulang_aot_receive_match_6",
                7 => "nulang_aot_receive_match_7",
                8 => "nulang_aot_receive_match_8",
                n => {
                    return Err(AotCompileError::Unsupported(format!(
                        "receive with {} candidate behaviors (max 8 in AOT)",
                        n
                    )))
                }
            };
            let call_args: Vec<Value> = behavior_ids
                .iter()
                .map(|id| builder.ins().iconst(types::I64, *id as i64))
                .collect();
            let arm_val = call_helper(builder, helpers, helper_name, &call_args)?;
            let dst_reg = mir::FunctionBuilder::LOCAL_BASE + dst.0;
            local_vals.insert(dst_reg, arm_val);
            // Payload temps are the contiguous locals dst+1 .. dst+max_params.
            for i in 0..*max_params {
                let idx_const = builder.ins().iconst(types::I64, i as i64);
                let pv = call_helper(builder, helpers, "nulang_aot_receive_payload", &[idx_const])?;
                local_vals.insert(dst_reg + 1 + i as u32, pv);
            }
            Ok(())
        }
        mir::Stmt::Assign { dst, op } => {
            // Resuming PerformDirect: an effect with a statically-resolved,
            // resuming handler compiles as intra-function continuation. The
            // perform jumps to the handler body block (with the effect args as
            // its block params); the handler body ends in `Terminator::Resume`,
            // which jumps back to a continuation block carrying the resume
            // value into `dst`. No native-stack capture is needed because the
            // handler body lives in the same compiled function.
            if let mir::RValue::Perform {
                args,
                resolved_handler: Some(href),
                ..
            } = op
            {
                if let Some(binding) = handler_tables
                    .get(href.table_index as usize)
                    .and_then(|t| t.bindings.get(href.binding_index as usize))
                {
                    if binding.resume {
                        let dst_reg = mir::FunctionBuilder::LOCAL_BASE + dst.0;
                        let body_block = binding.body;
                        // Continuation block receives the resume value into dst,
                        // plus (for a handler invoked from multiple perform
                        // sites) the prior perform results this site's tail may
                        // read — threaded through from earlier continuations.
                        // The threaded slot count is the handler body's UNIFORM
                        // width (max same-block priors across all sites); this
                        // site binds only the same-block priors it actually has,
                        // leaving the excess params unused.
                        let cont = builder.create_block();
                        builder.append_block_param(cont, types::I64);
                        let width = handler_threaded_width
                            .get(&body_block)
                            .copied()
                            .unwrap_or(0);
                        for _ in 0..width {
                            builder.append_block_param(cont, types::I64);
                        }
                        // Extra continuation live-ins this site must thread
                        // through the handler (values live into this site's
                        // continuation, minus its dst and same-block priors).
                        let extras: Vec<u32> = site_extras
                            .get(&(current_block, stmt_idx))
                            .cloned()
                            .unwrap_or_default();
                        // Jump to the handler body. The handler body's block
                        // params are its effect parameters (first, in declared
                        // order) then any merge live-ins; for a handler invoked
                        // from multiple perform sites a continuation-index param
                        // and the threaded prior-dest params follow. Effect args
                        // fill the effect params; live-ins are copied through; the
                        // index identifies which perform's continuation Resume
                        // must return to.
                        let handler_block = *block_map.get(&body_block).ok_or_else(|| {
                            AotCompileError::Internal(format!(
                                "handler body block {} not mapped",
                                body_block.0
                            ))
                        })?;
                        let effect_regs: Vec<u32> = binding
                            .params
                            .iter()
                            .map(|p| mir::FunctionBuilder::LOCAL_BASE + p.0)
                            .collect();
                        let param_regs = block_params.get(&body_block).cloned().unwrap_or_default();
                        let mut arg_iter = args.iter();
                        let mut jump_args: Vec<BlockArg> = Vec::with_capacity(param_regs.len() + 1);
                        for (i, reg) in param_regs.iter().enumerate() {
                            let v = if i < effect_regs.len() {
                                let arg = arg_iter.next().ok_or_else(|| {
                                    AotCompileError::Internal(format!(
                                        "fewer effect args than params for handler body {}",
                                        body_block.0
                                    ))
                                })?;
                                let arg_reg = mir::FunctionBuilder::LOCAL_BASE + arg.0;
                                *local_vals.get(&arg_reg).ok_or_else(|| {
                                    AotCompileError::Internal(format!(
                                        "perform arg local {} uninitialized",
                                        arg.0
                                    ))
                                })?
                            } else {
                                *local_vals.get(reg).ok_or_else(|| {
                                    AotCompileError::Internal(format!(
                                        "handler live-in local {} uninitialized at perform",
                                        reg
                                    ))
                                })?
                            };
                            jump_args.push(BlockArg::from(v));
                        }
                        let conts = handler_continuations.entry(body_block).or_default();
                        let idx = conts.len();
                        if multi_cont_bodies.contains(&body_block) {
                            jump_args
                                .push(BlockArg::from(builder.ins().iconst(types::I64, idx as i64)));
                        }
                        // Threaded slots: this site's prior perform results
                        // (real values, in order), then its extra continuation
                        // live-ins, then dummies for the rest up to the
                        // handler body's uniform width. Present whenever any
                        // site has continuation live-ins (width > 0), even for
                        // a single perform site (e.g. a loop-carried value).
                        let thread_total = handler_threaded_width
                            .get(&body_block)
                            .copied()
                            .unwrap_or(0);
                        for j in 0..thread_total {
                            let reg = if j < cont_thread.len() {
                                Some(cont_thread[j])
                            } else if j < cont_thread.len() + extras.len() {
                                Some(extras[j - cont_thread.len()])
                            } else {
                                None
                            };
                            let v = match reg {
                                Some(reg) => *local_vals.get(&reg).ok_or_else(|| {
                                    AotCompileError::Internal(format!(
                                        "threaded perform value local {} uninitialized",
                                        reg
                                    ))
                                })?,
                                None => builder.ins().iconst(types::I64, 0),
                            };
                            jump_args.push(BlockArg::from(v));
                        }
                        builder.ins().jump(handler_block, &jump_args);
                        // Continue the rest of this block in the continuation;
                        // dst = the resume value, prior results rebound to the
                        // threaded params.
                        builder.switch_to_block(cont);
                        let cparams = builder.block_params(cont);
                        local_vals.insert(dst_reg, cparams[0]);
                        for (j, reg) in cont_thread.iter().enumerate() {
                            local_vals.insert(*reg, cparams[1 + j]);
                        }
                        for (k, reg) in extras.iter().enumerate() {
                            local_vals.insert(*reg, cparams[1 + cont_thread.len() + k]);
                        }
                        conts.push((cont, dst_reg));
                        cont_thread.push(dst_reg);
                        return Ok(());
                    } else {
                        // Abortive perform: control transfers to the handler
                        // body and never returns. Jump to it with the effect
                        // args as its block params; the handler body computes
                        // the handle expression's value and `PopHandler +
                        // Jump(join)` merges it at the join block (whose
                        // live-ins already carry the merge). The remainder of
                        // this block (after the perform) is dead — bind dst to
                        // a dummy and let it compile into a fresh continuation
                        // so the block graph stays valid; the real value flows
                        // from the handler body path.
                        let dst_reg = mir::FunctionBuilder::LOCAL_BASE + dst.0;
                        let handler_block = *block_map.get(&binding.body).ok_or_else(|| {
                            AotCompileError::Internal(format!(
                                "abortive handler body block {} not mapped",
                                binding.body.0
                            ))
                        })?;
                        let param_regs =
                            block_params.get(&binding.body).cloned().unwrap_or_default();
                        let mut jump_args: Vec<BlockArg> = Vec::with_capacity(param_regs.len());
                        for (arg, _param_reg) in args.iter().zip(param_regs.iter()) {
                            let arg_reg = mir::FunctionBuilder::LOCAL_BASE + arg.0;
                            jump_args.push(BlockArg::from(*local_vals.get(&arg_reg).ok_or_else(
                                || {
                                    AotCompileError::Internal(format!(
                                        "perform arg local {} uninitialized",
                                        arg.0
                                    ))
                                },
                            )?));
                        }
                        builder.ins().jump(handler_block, &jump_args);
                        let cont = builder.create_block();
                        builder.switch_to_block(cont);
                        // Dead-value binding so the (dead) post-perform code
                        // compiles; never executed.
                        local_vals.insert(dst_reg, builder.ins().iconst(types::I64, 0));
                        return Ok(());
                    }
                }
            }
            if let mir::RValue::Closure { func, captures } = op {
                let reg = mir::FunctionBuilder::LOCAL_BASE + dst.0;
                closure_targets.insert(reg, *func);
                if !captures.is_empty() {
                    captured_closure_locals.insert(reg);
                }
            }
            let val = compile_rvalue(
                builder,
                op,
                type_meta,
                helpers,
                call_targets,
                closure_targets,
                captured_closure_locals,
                local_vals,
                mode,
                constants,
                field_map,
                foreign_functions,
            )?;
            let reg = mir::FunctionBuilder::LOCAL_BASE + dst.0;
            local_vals.insert(reg, val);
            Ok(())
        }
        mir::Stmt::EnterHandle { .. } | mir::Stmt::PopHandler => {
            // Handler tables and the handler stack are a runtime (VM)
            // concept — at the AOT level these are no-ops.  The handler
            // body is compiled inline as ordinary blocks.
            Ok(())
        }
        mir::Stmt::StoreFieldNamed { obj, field, src } => {
            let obj_reg = mir::FunctionBuilder::LOCAL_BASE + obj.0;
            let obj_val = *local_vals.get(&obj_reg).ok_or_else(|| {
                AotCompileError::Internal("StoreFieldNamed obj uninitialized".into())
            })?;
            let src_reg = mir::FunctionBuilder::LOCAL_BASE + src.0;
            let src_val = *local_vals.get(&src_reg).ok_or_else(|| {
                AotCompileError::Internal("StoreFieldNamed src uninitialized".into())
            })?;
            let slot = field_map.get(field).copied().unwrap_or(0);
            let slot_val = builder.ins().iconst(types::I64, slot as i64);
            call_void_helper(
                builder,
                helpers,
                "nulang_obj_set",
                &[obj_val, slot_val, src_val],
            )?;
            Ok(())
        }
        mir::Stmt::ArrayStore { arr, idx, src } => {
            let arr_reg = mir::FunctionBuilder::LOCAL_BASE + arr.0;
            let arr_val = *local_vals
                .get(&arr_reg)
                .ok_or_else(|| AotCompileError::Internal("ArrayStore arr uninitialized".into()))?;
            let idx_reg = mir::FunctionBuilder::LOCAL_BASE + idx.0;
            let idx_val = *local_vals
                .get(&idx_reg)
                .ok_or_else(|| AotCompileError::Internal("ArrayStore idx uninitialized".into()))?;
            let src_reg = mir::FunctionBuilder::LOCAL_BASE + src.0;
            let src_val = *local_vals
                .get(&src_reg)
                .ok_or_else(|| AotCompileError::Internal("ArrayStore src uninitialized".into()))?;
            call_void_helper(
                builder,
                helpers,
                "nulang_obj_set",
                &[arr_val, idx_val, src_val],
            )?;
            Ok(())
        }
        mir::Stmt::Emit { event, args } => {
            // `emit Event(args)` routes through `nulang_aot_emit_N`, which
            // calls the callbacks' `emit_event` to record the event on the
            // current actor (actor.event_log), matching the bytecode Emit
            // opcode. The event name is interned during the pre-scan.
            let event_val = compile_const(
                builder,
                &crate::bytecode::Constant::String(event.clone()),
                mode,
                constants,
            )?;
            let mut arg_vals = Vec::with_capacity(args.len());
            for a in args {
                let reg = mir::FunctionBuilder::LOCAL_BASE + a.0;
                arg_vals.push(local_vals.get(&reg).copied().ok_or_else(|| {
                    AotCompileError::Internal(format!("Emit arg local {} uninitialized", a.0))
                })?);
            }
            let helper_name = match arg_vals.len() {
                0 => "nulang_aot_emit_0",
                1 => "nulang_aot_emit_1",
                2 => "nulang_aot_emit_2",
                3 => "nulang_aot_emit_3",
                4 => "nulang_aot_emit_4",
                5 => "nulang_aot_emit_5",
                6 => "nulang_aot_emit_6",
                7 => "nulang_aot_emit_7",
                8 => "nulang_aot_emit_8",
                n => {
                    return Err(AotCompileError::Unsupported(format!(
                        "Emit with {} args (max 8 in AOT)",
                        n
                    )))
                }
            };
            let mut call_args = Vec::with_capacity(arg_vals.len() + 1);
            call_args.push(event_val);
            call_args.extend(arg_vals);
            call_void_helper(builder, helpers, helper_name, &call_args)?;
            Ok(())
        }
        mir::Stmt::StateSet { field, src } => {
            let c = crate::bytecode::Constant::String(field.clone());
            let field_val = compile_const(builder, &c, mode, constants)?;
            let src_reg = mir::FunctionBuilder::LOCAL_BASE + src.0;
            let src_val = *local_vals
                .get(&src_reg)
                .ok_or_else(|| AotCompileError::Internal("StateSet src uninitialized".into()))?;
            call_void_helper(
                builder,
                helpers,
                "nulang_aot_state_set",
                &[field_val, src_val],
            )?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// RValue compilation
// ---------------------------------------------------------------------------
fn compile_rvalue(
    builder: &mut FunctionBuilder,
    rv: &mir::RValue,
    type_meta: &TypeMetadata,
    helpers: &HashMap<&str, FuncRef>,
    call_targets: &HashMap<usize, FuncRef>,
    closure_targets: &mut HashMap<u32, usize>,
    captured_closure_locals: &HashSet<u32>,
    local_vals: &HashMap<u32, Value>,
    mode: CompileMode,
    constants: &[crate::bytecode::Constant],
    field_map: &HashMap<String, u8>,
    foreign_functions: &[mir::ForeignFunction],
) -> AotResult<Value> {
    match rv {
        mir::RValue::Const(c) => compile_const(builder, c, mode, constants),
        mir::RValue::Panic(_) => Err(AotCompileError::Unsupported(
            "Panic: contract violations require the bytecode backend (unavailable with --backend native)".into(),
        )),

        mir::RValue::Load(id) => {
            let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
            local_vals
                .get(&reg)
                .copied()
                .ok_or_else(|| AotCompileError::Internal(format!("uninitialized local {}", id.0)))
        }
        mir::RValue::Binary(op, lhs, rhs) => compile_binary(
            builder, *op, *lhs, *rhs, type_meta, helpers, local_vals, mode,
        ),

        mir::RValue::Unary(op, operand) => {
            compile_unary(builder, *op, *operand, type_meta, helpers, local_vals, mode)
        }

        mir::RValue::Call { func, args } => {
            let callee_ref = match func {
                mir::FuncRef::Index(n) => call_targets.get(n).copied().ok_or_else(|| {
                    AotCompileError::Internal(format!("call target fn {} not compiled yet", n))
                })?,
                mir::FuncRef::Local(closure_id) => {
                    let reg = mir::FunctionBuilder::LOCAL_BASE + closure_id.0;
                    match closure_targets.get(&reg) {
                        Some(&target_idx) if !captured_closure_locals.contains(&reg) => {
                            // Statically-known uncaptured closure: direct call.
                            call_targets.get(&target_idx).copied().ok_or_else(|| {
                                AotCompileError::Internal(format!(
                                    "call target fn {} not compiled yet",
                                    target_idx
                                ))
                            })?
                        }
                        _ => {
                            // Captured closure (statically known) or a closure
                            // value whose target is not statically known (e.g.
                            // passed as a parameter): route through the runtime
                            // helper, which dispatches on the value's tag
                            // (TAG_CLOSURE object → args + captures; TAG_INT →
                            // uncaptured fn index → args only). Mirrors the
                            // bytecode ClosureCall.
                            let closure_val = local_vals.get(&reg).ok_or_else(|| {
                                AotCompileError::Internal("closure value uninitialized".into())
                            })?;
                            let arg_vals: Vec<Value> =
                                args.iter()
                                    .map(|id| {
                                        let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
                                        local_vals.get(&reg).copied().ok_or_else(|| {
                                            AotCompileError::Internal(
                                                "call arg uninitialized".into(),
                                            )
                                        })
                                    })
                                    .collect::<AotResult<Vec<_>>>()?;
                            let helper_name = match arg_vals.len() {
                                0 => "nulang_aot_call_closure_0",
                                1 => "nulang_aot_call_closure_1",
                                2 => "nulang_aot_call_closure_2",
                                3 => "nulang_aot_call_closure_3",
                                4 => "nulang_aot_call_closure_4",
                                5 => "nulang_aot_call_closure_5",
                                6 => "nulang_aot_call_closure_6",
                                7 => "nulang_aot_call_closure_7",
                                8 => "nulang_aot_call_closure_8",
                                n => {
                                    return Err(AotCompileError::Unsupported(format!(
                                        "closure call with {} args (max 8 in AOT)",
                                        n
                                    )))
                                }
                            };
                            let mut call_args = Vec::with_capacity(arg_vals.len() + 1);
                            call_args.push(*closure_val);
                            call_args.extend(arg_vals);
                            return call_helper(builder, helpers, helper_name, &call_args);
                        }
                    }
                }
            };
            let arg_vals: Vec<Value> =
                args.iter()
                    .map(|id| {
                        let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
                        local_vals.get(&reg).copied().ok_or_else(|| {
                            AotCompileError::Internal("call arg uninitialized".into())
                        })
                    })
                    .collect::<AotResult<Vec<_>>>()?;
            let call = builder.ins().call(callee_ref, &arg_vals);
            Ok(builder.inst_results(call)[0])
        }

        mir::RValue::Closure { func, captures } => {
            if captures.is_empty() {
                // Return tagged function index — also register for call resolution.
                let idx = builder.ins().iconst(types::I64, *func as i64);
                Ok(emit_tag_int(builder, idx))
            } else {
                // Allocate a closure object carrying the captured values and
                // return it as a TAG_CLOSURE value. The lifted target function
                // receives the captures as trailing params when dispatched.
                let fn_val = builder.ins().iconst(types::I64, *func as i64);
                let cap_vals: Vec<Value> =
                    captures
                        .iter()
                        .map(|id| {
                            let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
                            local_vals.get(&reg).copied().ok_or_else(|| {
                                AotCompileError::Internal(format!(
                                    "closure capture local {} uninitialized",
                                    id.0
                                ))
                            })
                        })
                        .collect::<AotResult<Vec<_>>>()?;
                let helper_name = match cap_vals.len() {
                    0 => "nulang_aot_make_closure_0",
                    1 => "nulang_aot_make_closure_1",
                    2 => "nulang_aot_make_closure_2",
                    3 => "nulang_aot_make_closure_3",
                    4 => "nulang_aot_make_closure_4",
                    5 => "nulang_aot_make_closure_5",
                    6 => "nulang_aot_make_closure_6",
                    7 => "nulang_aot_make_closure_7",
                    8 => "nulang_aot_make_closure_8",
                    n => {
                        return Err(AotCompileError::Unsupported(format!(
                            "closure with {} captures (max 8 in AOT)",
                            n
                        )))
                    }
                };
                let mut call_args = Vec::with_capacity(cap_vals.len() + 1);
                call_args.push(fn_val);
                call_args.extend(cap_vals);
                call_helper(builder, helpers, helper_name, &call_args)
            }
        }

        mir::RValue::Perform {
            resolved_handler: Some(_),
            ..
        } => {
            // PerformDirect (statically-resolved handler) is not yet
            // supported in the AOT backend.  Use the bytecode backend
            // for effectful code, or the JIT which yields to the
            // interpreter for PerformDirect.
            Err(AotCompileError::Unsupported(
                "PerformDirect: effectful code requires the bytecode backend (unavailable with --backend native). \
                 Use --backend bytecode instead."
                    .into(),
            ))
        }
        mir::RValue::Perform {
            effect,
            op,
            args,
            resolved_handler: None,
        } => {
            // Builtin effect dispatch (no statically-resolved user handler):
            // route through `nulang_aot_perform_N`, which calls the callbacks'
            // `perform_builtin_effect_in_module` for IO/Actor/Timer/Test/Otp/
            // Http/Workflow builtins, matching the bytecode VM's unbound-
            // effect path. The effect/op strings are interned into the module
            // pool during the pre-scan, so `compile_const` emits resolvable
            // TAG_STRING constants.
            let eff_val = compile_const(
                builder,
                &crate::bytecode::Constant::String(effect.clone()),
                mode,
                constants,
            )?;
            let op_val = compile_const(
                builder,
                &crate::bytecode::Constant::String(op.clone()),
                mode,
                constants,
            )?;
            let mut arg_vals = Vec::with_capacity(args.len());
            for a in args {
                let reg = mir::FunctionBuilder::LOCAL_BASE + a.0;
                arg_vals.push(local_vals.get(&reg).copied().ok_or_else(|| {
                    AotCompileError::Internal(format!("Perform arg local {} uninitialized", a.0))
                })?);
            }
            let helper_name = match arg_vals.len() {
                0 => "nulang_aot_perform_0",
                1 => "nulang_aot_perform_1",
                2 => "nulang_aot_perform_2",
                3 => "nulang_aot_perform_3",
                4 => "nulang_aot_perform_4",
                5 => "nulang_aot_perform_5",
                6 => "nulang_aot_perform_6",
                7 => "nulang_aot_perform_7",
                8 => "nulang_aot_perform_8",
                n => {
                    return Err(AotCompileError::Unsupported(format!(
                        "Perform with {} args (max 8 in AOT)",
                        n
                    )))
                }
            };
            let mut call_args = Vec::with_capacity(arg_vals.len() + 2);
            call_args.push(eff_val);
            call_args.push(op_val);
            call_args.extend(arg_vals);
            call_helper(builder, helpers, helper_name, &call_args)
        }

        // ---- Record ----
        mir::RValue::Record(fields) => {
            let max_slot: u8 = fields
                .iter()
                .filter_map(|(name, _)| field_map.get(name))
                .copied()
                .max()
                .unwrap_or(0);
            let slot_count = (max_slot as u64).saturating_add(1);
            // alloc_obj(slot_count, type_tag=3 for Record)
            let count_val = builder.ins().iconst(types::I64, slot_count as i64);
            let tag_val = builder.ins().iconst(types::I32, 3);
            let ptr = call_helper(builder, helpers, "nulang_alloc_obj", &[count_val, tag_val])?;

            for (name, val_id) in fields {
                let val_reg = mir::FunctionBuilder::LOCAL_BASE + val_id.0;
                let val_val = *local_vals.get(&val_reg).ok_or_else(|| {
                    AotCompileError::Internal("record field uninitialized".into())
                })?;
                let slot = field_map.get(name).copied().unwrap_or(0);
                let slot_val = builder.ins().iconst(types::I64, slot as i64);
                call_void_helper(
                    builder,
                    helpers,
                    "nulang_obj_set",
                    &[ptr, slot_val, val_val],
                )?;
            }
            Ok(ptr)
        }

        // ---- Tuple ----
        mir::RValue::Tuple(elements) => {
            let count = elements.len() as u64;
            let count_val = builder.ins().iconst(types::I64, count as i64);
            let tag_val = builder.ins().iconst(types::I32, 6);
            let ptr = call_helper(builder, helpers, "nulang_alloc_obj", &[count_val, tag_val])?;

            for (i, val_id) in elements.iter().enumerate() {
                let val_reg = mir::FunctionBuilder::LOCAL_BASE + val_id.0;
                let val_val = *local_vals.get(&val_reg).ok_or_else(|| {
                    AotCompileError::Internal("tuple element uninitialized".into())
                })?;
                let idx_val = builder.ins().iconst(types::I64, i as i64);
                call_void_helper(builder, helpers, "nulang_obj_set", &[ptr, idx_val, val_val])?;
            }
            Ok(ptr)
        }

        // ---- ArrayLit ----
        mir::RValue::ArrayLit(elements) => {
            let count = elements.len() as u64;
            let count_val = builder.ins().iconst(types::I64, count as i64);
            let tag_val = builder.ins().iconst(types::I32, 1);
            let ptr = call_helper(builder, helpers, "nulang_alloc_obj", &[count_val, tag_val])?;

            for (i, val_id) in elements.iter().enumerate() {
                let val_reg = mir::FunctionBuilder::LOCAL_BASE + val_id.0;
                let val_val = *local_vals.get(&val_reg).ok_or_else(|| {
                    AotCompileError::Internal("array element uninitialized".into())
                })?;
                let idx_val = builder.ins().iconst(types::I64, i as i64);
                call_void_helper(builder, helpers, "nulang_obj_set", &[ptr, idx_val, val_val])?;
            }
            Ok(ptr)
        }

        // ---- LoadFieldNamed (record field access) ----
        mir::RValue::LoadFieldNamed { obj, field } => {
            let obj_reg = mir::FunctionBuilder::LOCAL_BASE + obj.0;
            let obj_val = *local_vals.get(&obj_reg).ok_or_else(|| {
                AotCompileError::Internal("LoadFieldNamed obj uninitialized".into())
            })?;
            let slot = field_map.get(field).copied().unwrap_or(0);
            let slot_val = builder.ins().iconst(types::I64, slot as i64);
            call_helper(builder, helpers, "nulang_obj_get", &[obj_val, slot_val])
        }

        // ---- LoadFieldPos (tuple field access) ----
        mir::RValue::LoadFieldPos { obj, index } => {
            let obj_reg = mir::FunctionBuilder::LOCAL_BASE + obj.0;
            let obj_val = *local_vals.get(&obj_reg).ok_or_else(|| {
                AotCompileError::Internal("LoadFieldPos obj uninitialized".into())
            })?;
            let idx_val = builder.ins().iconst(types::I64, *index as i64);
            call_helper(builder, helpers, "nulang_obj_get", &[obj_val, idx_val])
        }

        // ---- ArrayLoad ----
        mir::RValue::ArrayLoad { arr, idx } => {
            let arr_reg = mir::FunctionBuilder::LOCAL_BASE + arr.0;
            let arr_val = *local_vals
                .get(&arr_reg)
                .ok_or_else(|| AotCompileError::Internal("ArrayLoad arr uninitialized".into()))?;
            let idx_reg = mir::FunctionBuilder::LOCAL_BASE + idx.0;
            let idx_val = *local_vals
                .get(&idx_reg)
                .ok_or_else(|| AotCompileError::Internal("ArrayLoad idx uninitialized".into()))?;
            call_helper(builder, helpers, "nulang_obj_get", &[arr_val, idx_val])
        }

        // ---- ArrayLen ----
        mir::RValue::ArrayLen(arr) => {
            let arr_reg = mir::FunctionBuilder::LOCAL_BASE + arr.0;
            let arr_val = *local_vals
                .get(&arr_reg)
                .ok_or_else(|| AotCompileError::Internal("ArrayLen arr uninitialized".into()))?;
            call_helper(builder, helpers, "nulang_obj_len", &[arr_val])
        }

        // ---- StringEq ----
        mir::RValue::StringEq(l, r) => {
            let l_reg = mir::FunctionBuilder::LOCAL_BASE + l.0;
            let l_val = *local_vals
                .get(&l_reg)
                .ok_or_else(|| AotCompileError::Internal("StringEq lhs uninitialized".into()))?;
            let r_reg = mir::FunctionBuilder::LOCAL_BASE + r.0;
            let r_val = *local_vals
                .get(&r_reg)
                .ok_or_else(|| AotCompileError::Internal("StringEq rhs uninitialized".into()))?;
            call_helper(builder, helpers, "nulang_str_eq", &[l_val, r_val])
        }

        // ---- StrConcat ----
        mir::RValue::StrConcat(l, r) => {
            let l_reg = mir::FunctionBuilder::LOCAL_BASE + l.0;
            let l_val = *local_vals
                .get(&l_reg)
                .ok_or_else(|| AotCompileError::Internal("StrConcat lhs uninitialized".into()))?;
            let r_reg = mir::FunctionBuilder::LOCAL_BASE + r.0;
            let r_val = *local_vals
                .get(&r_reg)
                .ok_or_else(|| AotCompileError::Internal("StrConcat rhs uninitialized".into()))?;
            call_helper(builder, helpers, "nulang_str_concat", &[l_val, r_val])
        }

        // ---- RecordUpdate ----
        mir::RValue::RecordUpdate { base, overrides } => {
            let base_reg = mir::FunctionBuilder::LOCAL_BASE + base.0;
            let base_val = *local_vals.get(&base_reg).ok_or_else(|| {
                AotCompileError::Internal("RecordUpdate base uninitialized".into())
            })?;
            let copy = call_helper(builder, helpers, "nulang_rec_copy", &[base_val])?;
            for (name, val_id) in overrides {
                let val_reg = mir::FunctionBuilder::LOCAL_BASE + val_id.0;
                let val_val = *local_vals.get(&val_reg).ok_or_else(|| {
                    AotCompileError::Internal("RecordUpdate override uninitialized".into())
                })?;
                let slot = field_map.get(name).copied().unwrap_or(0);
                let slot_val = builder.ins().iconst(types::I64, slot as i64);
                call_void_helper(
                    builder,
                    helpers,
                    "nulang_obj_set",
                    &[copy, slot_val, val_val],
                )?;
            }
            Ok(copy)
        }
        mir::RValue::PerformAsync { effect_op, args, .. } => {
            // Async-effect dispatch: route through `nulang_aot_perform_async_N`,
            // which calls the callbacks' `perform_async` (the same path the
            // bytecode PerformAsync opcode takes). Synchronously-completing
            // effects (Pipeline.*, Supervisor.*, Timer.sleep(0)) deliver their
            // result; suspending effects (LLM/Inference.ask, Timer.sleep with
            // a positive delay) degrade to nil — the native backend has no VM
            // suspension to park the actor mid-behavior.
            let eff_val = compile_const(
                builder,
                &crate::bytecode::Constant::String(effect_op.clone()),
                mode,
                constants,
            )?;
            let arg_vals: Vec<Value> = args
                .iter()
                .map(|id| {
                    let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
                    local_vals.get(&reg).copied().ok_or_else(|| {
                        AotCompileError::Internal(format!(
                            "perform_async arg local {} uninitialized",
                            id.0
                        ))
                    })
                })
                .collect::<AotResult<Vec<_>>>()?;
            let helper_name = match arg_vals.len() {
                0 => "nulang_aot_perform_async_0",
                1 => "nulang_aot_perform_async_1",
                2 => "nulang_aot_perform_async_2",
                3 => "nulang_aot_perform_async_3",
                4 => "nulang_aot_perform_async_4",
                5 => "nulang_aot_perform_async_5",
                6 => "nulang_aot_perform_async_6",
                7 => "nulang_aot_perform_async_7",
                8 => "nulang_aot_perform_async_8",
                n => {
                    return Err(AotCompileError::Unsupported(format!(
                        "perform_async with {} args (max 8 in AOT)",
                        n
                    )))
                }
            };
            let mut call_args = Vec::with_capacity(arg_vals.len() + 1);
            call_args.push(eff_val);
            call_args.extend(arg_vals);
            call_helper(builder, helpers, helper_name, &call_args)
        }
        mir::RValue::SignalWait { name } => {
            // Workflow signal wait: route through `nulang_aot_signal_wait`,
            // which calls the callbacks' `wait_signal` (the same path the
            // bytecode SignalWait opcode takes). A ready signal delivers its
            // value (and outside a workflow the default callback delivers
            // unit); a signal that has not been received degrades to nil —
            // the native backend has no VM suspension.
            let name_val = compile_const(
                builder,
                &crate::bytecode::Constant::String(name.clone()),
                mode,
                constants,
            )?;
            call_helper(builder, helpers, "nulang_aot_signal_wait", &[name_val])
        }
        // Handled by the Assign arm (multi-register write); never reached here.
        mir::RValue::ReceiveMatch { .. } | mir::RValue::ReceiveWait { .. } => {
            Err(AotCompileError::Internal(
                "ReceiveMatch/ReceiveWait must be compiled via the Assign arm".into(),
            ))
        }
        mir::RValue::ReceiveCommit => Err(AotCompileError::Unsupported(
            "ReceiveCommit: receive commit requires the bytecode backend (unavailable with --backend native)".into(),
        )),
        mir::RValue::FFICall { idx, args } => {
            let def = foreign_functions.get(*idx).ok_or_else(|| {
                AotCompileError::Internal(format!(
                    "FFICall: foreign function {} not declared",
                    idx
                ))
            })?;
            // Map declared Nulang types to C ABI types.
            let mut params: Vec<crate::ffi::marshal::CType> =
                Vec::with_capacity(def.params.len());
            for p in &def.params {
                let ctype = crate::ffi::marshal::ffi_type_to_ctype(
                    &crate::ffi::marshal::nulang_type_to_ffi_type(p).ok_or_else(|| {
                        AotCompileError::Unsupported(format!(
                            "FFICall: unsupported parameter type for {}",
                            def.symbol
                        ))
                    })?,
                )
                .ok_or_else(|| {
                    AotCompileError::Unsupported(format!(
                        "FFICall: unsupported parameter type for {}",
                        def.symbol
                    ))
                })?;
                if ctype == crate::ffi::marshal::CType::VoidPtr {
                    return Err(AotCompileError::Unsupported(format!(
                        "FFICall: VoidPtr parameter for {} (unsupported in AOT)",
                        def.symbol
                    )));
                }
                params.push(ctype);
            }
            let ret = crate::ffi::marshal::ffi_type_to_ctype(
                &crate::ffi::marshal::nulang_type_to_ffi_type(&def.ret).ok_or_else(|| {
                    AotCompileError::Unsupported(format!(
                        "FFICall: unsupported return type for {}",
                        def.symbol
                    ))
                })?,
            )
            .ok_or_else(|| {
                AotCompileError::Unsupported(format!(
                    "FFICall: unsupported return type for {}",
                    def.symbol
                ))
            })?;
            if ret == crate::ffi::marshal::CType::CStr {
                return Err(AotCompileError::Unsupported(format!(
                    "FFICall: C string returns for {} (unsupported in AOT)",
                    def.symbol
                )));
            }
            if args.len() > 4 {
                return Err(AotCompileError::Unsupported(format!(
                    "FFICall: {} args (max 4 in AOT)",
                    args.len()
                )));
            }
            if args.len() != def.params.len() {
                return Err(AotCompileError::Internal(format!(
                    "FFICall: {} args but {} declared params for {}",
                    args.len(),
                    def.params.len(),
                    def.symbol
                )));
            }
            // Bit-pack the signature: low 3 bits = return CType tag, then 3
            // bits per parameter (I64=0, F64=1, Bool=2, CStr=3, VoidPtr=4,
            // Unit=5).
            let ctype_tag = |c: crate::ffi::marshal::CType| -> u64 {
                match c {
                    crate::ffi::marshal::CType::I64 => 0,
                    crate::ffi::marshal::CType::F64 => 1,
                    crate::ffi::marshal::CType::Bool => 2,
                    crate::ffi::marshal::CType::CStr => 3,
                    crate::ffi::marshal::CType::VoidPtr => 4,
                    crate::ffi::marshal::CType::Unit => 5,
                }
            };
            let mut sig: u64 = ctype_tag(ret);
            for (i, c) in params.iter().enumerate() {
                sig |= ctype_tag(*c) << (3 + 3 * i);
            }
            // Library and symbol are interned into the constant pool during
            // the pre-scan, so compile_const resolves their pool indices and
            // the helper recovers their content via resolve_string_coerce.
            let lib_val = compile_const(
                builder,
                &crate::bytecode::Constant::String(def.library.clone()),
                mode,
                constants,
            )?;
            let sym_val = compile_const(
                builder,
                &crate::bytecode::Constant::String(def.symbol.clone()),
                mode,
                constants,
            )?;
            let sig_val = builder.ins().iconst(types::I64, sig as i64);
            let arg_vals: Vec<Value> = args
                .iter()
                .map(|id| {
                    let reg = mir::FunctionBuilder::LOCAL_BASE + id.0;
                    local_vals.get(&reg).copied().ok_or_else(|| {
                        AotCompileError::Internal(format!(
                            "FFICall arg local {} uninitialized",
                            id.0
                        ))
                    })
                })
                .collect::<AotResult<Vec<_>>>()?;
            let helper_name = match arg_vals.len() {
                0 => "nulang_aot_ffi_call_0",
                1 => "nulang_aot_ffi_call_1",
                2 => "nulang_aot_ffi_call_2",
                3 => "nulang_aot_ffi_call_3",
                4 => "nulang_aot_ffi_call_4",
                _ => unreachable!(),
            };
            let mut call_args = Vec::with_capacity(arg_vals.len() + 3);
            call_args.push(lib_val);
            call_args.push(sym_val);
            call_args.push(sig_val);
            call_args.extend(arg_vals);
            call_helper(builder, helpers, helper_name, &call_args)
        }
        mir::RValue::Migrate { actor, node } => {
            // `migrate actor to node`: the native backend has no distribution
            // layer, so the request is a no-op delivering unit — the same
            // contract as the bytecode VM without distributed callbacks armed.
            let actor_val = {
                let reg = mir::FunctionBuilder::LOCAL_BASE + actor.0;
                *local_vals.get(&reg).ok_or_else(|| {
                    AotCompileError::Internal(format!(
                        "migrate actor local {} uninitialized",
                        actor.0
                    ))
                })?
            };
            let node_val = {
                let reg = mir::FunctionBuilder::LOCAL_BASE + node.0;
                *local_vals.get(&reg).ok_or_else(|| {
                    AotCompileError::Internal(format!(
                        "migrate node local {} uninitialized",
                        node.0
                    ))
                })?
            };
            call_helper(builder, helpers, "nulang_aot_migrate", &[actor_val, node_val])
        }
        mir::RValue::Receive => call_helper(builder, helpers, "nulang_aot_receive_pop", &[]),
        mir::RValue::SelfRef => call_helper(builder, helpers, "nulang_aot_self_ref", &[]),
        mir::RValue::CapabilityCheck { .. } => {
            // Capabilities are compile-time only; the check is always true at
            // runtime (the bytecode backend emits Const1 for this opcode).
            Ok(builder.ins().iconst(types::I64, TAG_BOOL_I64 | 1))
        }
        mir::RValue::StateGet { field } => {
            let c = crate::bytecode::Constant::String(field.clone());
            let field_val = compile_const(builder, &c, mode, constants)?;
            call_helper(builder, helpers, "nulang_aot_state_get", &[field_val])
        }
        mir::RValue::Spawn {
            behavior_idx,
            init,
            target_node,
            capabilities: _,
        } => {
            if target_node.is_some() {
                return Err(AotCompileError::Unsupported(
                    "Spawn: remote spawn (spawn@node) requires distribution (unavailable with --backend native)"
                        .into(),
                ));
            }
            // Queue each init pair: (name const idx, value).
            for (name, val_rv) in init {
                let name_idx = constants
                    .iter()
                    .position(|k| matches!(k, crate::bytecode::Constant::String(s) if s == name))
                    .unwrap_or(0) as i64;
                let val = compile_rvalue(
                    builder,
                    val_rv,
                    type_meta,
                    helpers,
                    call_targets,
                    closure_targets,
                    captured_closure_locals,
                    local_vals,
                    mode,
                    constants,
                    field_map,
                    foreign_functions,
                )?;
                let name_val = builder.ins().iconst(types::I64, name_idx);
                call_void_helper(builder, helpers, "nulang_aot_spawn_push", &[name_val, val])?;
            }
            let behavior_val = builder.ins().iconst(types::I64, *behavior_idx as i64);
            call_helper(builder, helpers, "nulang_aot_spawn", &[behavior_val])
        }
        mir::RValue::Send {
            actor,
            behavior_idx,
            args,
            ..
        } => {
            // target actor ref
            let actor_reg = mir::FunctionBuilder::LOCAL_BASE + actor.0;
            let target = local_vals.get(&actor_reg).copied().ok_or_else(|| {
                AotCompileError::Internal(format!("Send target local {} uninitialized", actor.0))
            })?;
            let behavior_val = builder.ins().iconst(types::I64, *behavior_idx as i64);
            let mut arg_vals = Vec::with_capacity(args.len());
            for a in args {
                let reg = mir::FunctionBuilder::LOCAL_BASE + a.0;
                arg_vals.push(local_vals.get(&reg).copied().ok_or_else(|| {
                    AotCompileError::Internal(format!("Send arg local {} uninitialized", a.0))
                })?);
            }
            let helper_name = match arg_vals.len() {
                0 => "nulang_aot_send_0",
                1 => "nulang_aot_send_1",
                2 => "nulang_aot_send_2",
                3 => "nulang_aot_send_3",
                4 => "nulang_aot_send_4",
                5 => "nulang_aot_send_5",
                6 => "nulang_aot_send_6",
                7 => "nulang_aot_send_7",
                8 => "nulang_aot_send_8",
                n => {
                    return Err(AotCompileError::Unsupported(format!(
                        "Send with {} args (max 8 in AOT)",
                        n
                    )))
                }
            };
            let mut call_args = Vec::with_capacity(arg_vals.len() + 2);
            call_args.push(target);
            call_args.push(behavior_val);
            call_args.extend(arg_vals);
            call_void_helper(builder, helpers, helper_name, &call_args)?;
            // `send` evaluates to unit; emit the unit constant.
            Ok(builder.ins().iconst(types::I64, crate::value_layout::TAG_UNIT as i64))
        }
        mir::RValue::Ask {
            actor,
            behavior_idx,
            args,
            remote,
            ..
        } => {
            if *remote {
                return Err(AotCompileError::Unsupported(
                    "Ask: remote ask requires distribution (unavailable with --backend native)"
                        .into(),
                ));
            }
            if args.len() > 8 {
                return Err(AotCompileError::Unsupported(format!(
                    "Ask with {} args (max 8 in AOT)",
                    args.len()
                )));
            }
            let actor_reg = mir::FunctionBuilder::LOCAL_BASE + actor.0;
            let target = local_vals.get(&actor_reg).copied().ok_or_else(|| {
                AotCompileError::Internal(format!("Ask actor local {} uninitialized", actor.0))
            })?;
            let behavior_val = builder.ins().iconst(types::I64, *behavior_idx as i64);
            let mut arg_vals = Vec::with_capacity(args.len());
            for a in args {
                let reg = mir::FunctionBuilder::LOCAL_BASE + a.0;
                arg_vals.push(local_vals.get(&reg).copied().ok_or_else(|| {
                    AotCompileError::Internal(format!("Ask arg local {} uninitialized", a.0))
                })?);
            }
            let mut call_args = Vec::with_capacity(arg_vals.len() + 2);
            call_args.push(target);
            call_args.push(behavior_val);
            call_args.extend(arg_vals);
            let helper = format!("nulang_aot_ask_{}", args.len());
            call_helper(builder, helpers, &helper, &call_args)
        }
        mir::RValue::Resume(..) => Err(AotCompileError::Unsupported(
            "Resume: effect-continuation resume requires the bytecode backend (unavailable with --backend native)".into(),
        )),
    }
}
// ---------------------------------------------------------------------------
// Constant emission
// ---------------------------------------------------------------------------

fn compile_const(
    builder: &mut FunctionBuilder,
    c: &crate::bytecode::Constant,
    mode: CompileMode,
    constants: &[crate::bytecode::Constant],
) -> AotResult<Value> {
    match c {
        crate::bytecode::Constant::Int(v) => {
            if mode == CompileMode::Unboxed {
                Ok(builder.ins().iconst(types::I64, *v))
            } else {
                let iconst_val = builder.ins().iconst(types::I64, *v);
                Ok(emit_tag_int(builder, iconst_val))
            }
        }
        crate::bytecode::Constant::Float(f) => {
            Ok(builder.ins().iconst(types::I64, f.to_bits() as i64))
        }
        crate::bytecode::Constant::Bool(b) => Ok(builder
            .ins()
            .iconst(types::I64, TAG_BOOL_I64 | if *b { 1 } else { 0 })),
        crate::bytecode::Constant::Unit => Ok(builder
            .ins()
            .iconst(types::I64, 0x7FF9_0000_0000_0000u64 as i64)),
        crate::bytecode::Constant::Nil => Ok(builder.ins().iconst(types::I64, TAG_NIL_I64)),
        crate::bytecode::Constant::String(_) => {
            // Emit TAG_STRING | index into constant pool.
            // The constant pool is built during module pre-scan.
            let idx = constants.iter().position(|k| k == c).unwrap_or(0);
            Ok(builder.ins().iconst(
                types::I64,
                (crate::value_layout::TAG_STRING | idx as u64) as i64,
            ))
        }
        crate::bytecode::Constant::TypeDescriptor(_) => Err(AotCompileError::Unsupported(
            "TypeDescriptor constant".into(),
        )),
        crate::bytecode::Constant::FunctionRef(_) => {
            Err(AotCompileError::Unsupported("FunctionRef constant".into()))
        }
        crate::bytecode::Constant::BehaviorRef(_) => {
            Err(AotCompileError::Unsupported("BehaviorRef constant".into()))
        }
    }
}

// ---------------------------------------------------------------------------
// Binary operation emission
// ---------------------------------------------------------------------------

fn compile_binary(
    builder: &mut FunctionBuilder,
    op: crate::ast::BinOp,
    lhs: mir::LocalId,
    rhs: mir::LocalId,
    type_meta: &TypeMetadata,
    helpers: &HashMap<&str, FuncRef>,
    local_vals: &HashMap<u32, Value>,
    mode: CompileMode,
) -> AotResult<Value> {
    let lhs_reg = mir::FunctionBuilder::LOCAL_BASE + lhs.0;
    let rhs_reg = mir::FunctionBuilder::LOCAL_BASE + rhs.0;
    let lhs_val = *local_vals
        .get(&lhs_reg)
        .ok_or_else(|| AotCompileError::Internal("uninitialized lhs".into()))?;
    let rhs_val = *local_vals
        .get(&rhs_reg)
        .ok_or_else(|| AotCompileError::Internal("uninitialized rhs".into()))?;

    use crate::ast::BinOp;
    // `TypeMetadata` is conservative for locals produced by control-flow
    // merges. In an all-Int unboxed function those locals still carry raw
    // integers; falling through to the boxed helper path would reinterpret
    // them as NaN-tagged values. Use the unboxed operation whenever the
    // enclosing function has selected the unboxed representation.
    if mode == CompileMode::Unboxed {
        let raw = match op {
            BinOp::Add => Some(builder.ins().iadd(lhs_val, rhs_val)),
            BinOp::Sub => Some(builder.ins().isub(lhs_val, rhs_val)),
            BinOp::Mul => Some(builder.ins().imul(lhs_val, rhs_val)),
            BinOp::Eq => Some({
                let cmp = builder.ins().icmp(IntCC::Equal, lhs_val, rhs_val);
                emit_tag_bool(builder, cmp)
            }),
            BinOp::Ne => Some({
                let cmp = builder.ins().icmp(IntCC::NotEqual, lhs_val, rhs_val);
                emit_tag_bool(builder, cmp)
            }),
            BinOp::Lt => Some({
                let cmp = builder.ins().icmp(IntCC::SignedLessThan, lhs_val, rhs_val);
                emit_tag_bool(builder, cmp)
            }),
            BinOp::Le => Some({
                let cmp = builder
                    .ins()
                    .icmp(IntCC::SignedLessThanOrEqual, lhs_val, rhs_val);
                emit_tag_bool(builder, cmp)
            }),
            BinOp::Gt => Some({
                let cmp = builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThan, lhs_val, rhs_val);
                emit_tag_bool(builder, cmp)
            }),
            BinOp::Ge => Some({
                let cmp = builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThanOrEqual, lhs_val, rhs_val);
                emit_tag_bool(builder, cmp)
            }),
            BinOp::BitAnd => Some(builder.ins().band(lhs_val, rhs_val)),
            BinOp::BitOr => Some(builder.ins().bor(lhs_val, rhs_val)),
            BinOp::BitXor => Some(builder.ins().bxor(lhs_val, rhs_val)),
            BinOp::Shl => Some(builder.ins().ishl(lhs_val, rhs_val)),
            BinOp::Shr => Some(builder.ins().sshr(lhs_val, rhs_val)),
            _ => None,
        };
        if let Some(value) = raw {
            return Ok(value);
        }
    }

    let lhs_reg_usize = lhs_reg as usize;
    let rhs_reg_usize = rhs_reg as usize;

    match op {
        BinOp::Add => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().iadd(lhs_val, rhs_val))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let sum = builder.ins().iadd(l, r);
                    Ok(emit_tag_int(builder, sum))
                }
            } else if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Float) {
                let l = builder.ins().bitcast(types::F64, MemFlags::new(), lhs_val);
                let r = builder.ins().bitcast(types::F64, MemFlags::new(), rhs_val);
                let sum = builder.ins().fadd(l, r);
                Ok(builder.ins().bitcast(types::I64, MemFlags::new(), sum))
            } else {
                // Fall back to runtime helper.
                call_helper(builder, helpers, "nulang_iadd", &[lhs_val, rhs_val])
            }
        }
        BinOp::Sub => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().isub(lhs_val, rhs_val))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let diff = builder.ins().isub(l, r);
                    Ok(emit_tag_int(builder, diff))
                }
            } else if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Float) {
                let l = builder.ins().bitcast(types::F64, MemFlags::new(), lhs_val);
                let r = builder.ins().bitcast(types::F64, MemFlags::new(), rhs_val);
                let diff = builder.ins().fsub(l, r);
                Ok(builder.ins().bitcast(types::I64, MemFlags::new(), diff))
            } else {
                call_helper(builder, helpers, "nulang_isub", &[lhs_val, rhs_val])
            }
        }
        BinOp::Mul => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().imul(lhs_val, rhs_val))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let prod = builder.ins().imul(l, r);
                    Ok(emit_tag_int(builder, prod))
                }
            } else if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Float) {
                let l = builder.ins().bitcast(types::F64, MemFlags::new(), lhs_val);
                let r = builder.ins().bitcast(types::F64, MemFlags::new(), rhs_val);
                let prod = builder.ins().fmul(l, r);
                Ok(builder.ins().bitcast(types::I64, MemFlags::new(), prod))
            } else {
                call_helper(builder, helpers, "nulang_imul", &[lhs_val, rhs_val])
            }
        }
        BinOp::Div => {
            // Route through `nulang_idiv` ALWAYS (not just for unknown/float
            // operands): it checks for a zero divisor BEFORE dividing and
            // returns nil, matching the interpreter. The old inline `sdiv` +
            // `select` computed the division unconditionally, so a zero
            // divisor TRAPPED (SIGILL) before the select could return nil.
            call_helper(builder, helpers, "nulang_idiv", &[lhs_val, rhs_val])
        }
        BinOp::Mod => {
            // Route through `nulang_imod` always for the same reason: `srem`
            // by zero traps, so the division must be guarded in the helper.
            call_helper(builder, helpers, "nulang_imod", &[lhs_val, rhs_val])
        }
        BinOp::Eq => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    let cmp = builder.ins().icmp(IntCC::Equal, lhs_val, rhs_val);
                    Ok(emit_tag_bool(builder, cmp))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let cmp = builder.ins().icmp(IntCC::Equal, l, r);
                    Ok(emit_tag_bool(builder, cmp))
                }
            } else {
                call_helper(builder, helpers, "nulang_icmp_eq", &[lhs_val, rhs_val])
            }
        }
        BinOp::Lt => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    let cmp = builder.ins().icmp(IntCC::SignedLessThan, lhs_val, rhs_val);
                    Ok(emit_tag_bool(builder, cmp))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let cmp = builder.ins().icmp(IntCC::SignedLessThan, l, r);
                    Ok(emit_tag_bool(builder, cmp))
                }
            } else {
                call_helper(builder, helpers, "nulang_icmp_lt", &[lhs_val, rhs_val])
            }
        }
        BinOp::Gt => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    let cmp = builder
                        .ins()
                        .icmp(IntCC::SignedGreaterThan, lhs_val, rhs_val);
                    Ok(emit_tag_bool(builder, cmp))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let cmp = builder.ins().icmp(IntCC::SignedGreaterThan, l, r);
                    Ok(emit_tag_bool(builder, cmp))
                }
            } else {
                call_helper(builder, helpers, "nulang_icmp_gt", &[lhs_val, rhs_val])
            }
        }
        BinOp::Le => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    let cmp = builder
                        .ins()
                        .icmp(IntCC::SignedLessThanOrEqual, lhs_val, rhs_val);
                    Ok(emit_tag_bool(builder, cmp))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let cmp = builder.ins().icmp(IntCC::SignedLessThanOrEqual, l, r);
                    Ok(emit_tag_bool(builder, cmp))
                }
            } else {
                call_helper(builder, helpers, "nulang_icmp_le", &[lhs_val, rhs_val])
            }
        }
        BinOp::Ge => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    let cmp = builder
                        .ins()
                        .icmp(IntCC::SignedGreaterThanOrEqual, lhs_val, rhs_val);
                    Ok(emit_tag_bool(builder, cmp))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let cmp = builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, l, r);
                    Ok(emit_tag_bool(builder, cmp))
                }
            } else {
                call_helper(builder, helpers, "nulang_icmp_ge", &[lhs_val, rhs_val])
            }
        }
        BinOp::Ne => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    let cmp = builder.ins().icmp(IntCC::NotEqual, lhs_val, rhs_val);
                    Ok(emit_tag_bool(builder, cmp))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let cmp = builder.ins().icmp(IntCC::NotEqual, l, r);
                    Ok(emit_tag_bool(builder, cmp))
                }
            } else {
                call_helper(builder, helpers, "nulang_icmp_eq", &[lhs_val, rhs_val])
                    .and_then(|eq| call_helper(builder, helpers, "nulang_not", &[eq]))
            }
        }
        BinOp::And => call_helper(builder, helpers, "nulang_and", &[lhs_val, rhs_val]),
        BinOp::Or => call_helper(builder, helpers, "nulang_or", &[lhs_val, rhs_val]),
        BinOp::BitAnd => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().band(lhs_val, rhs_val))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let result = builder.ins().band(l, r);
                    Ok(emit_tag_int(builder, result))
                }
            } else {
                call_helper(builder, helpers, "nulang_bitand", &[lhs_val, rhs_val])
            }
        }
        BinOp::BitOr => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().bor(lhs_val, rhs_val))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let result = builder.ins().bor(l, r);
                    Ok(emit_tag_int(builder, result))
                }
            } else {
                call_helper(builder, helpers, "nulang_bitor", &[lhs_val, rhs_val])
            }
        }
        BinOp::BitXor => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().bxor(lhs_val, rhs_val))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let result = builder.ins().bxor(l, r);
                    Ok(emit_tag_int(builder, result))
                }
            } else {
                call_helper(builder, helpers, "nulang_xor", &[lhs_val, rhs_val])
            }
        }
        BinOp::Shl => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().ishl(lhs_val, rhs_val))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let result = builder.ins().ishl(l, r);
                    Ok(emit_tag_int(builder, result))
                }
            } else {
                call_helper(builder, helpers, "nulang_shl", &[lhs_val, rhs_val])
            }
        }
        BinOp::Shr => {
            if type_meta.both_known(lhs_reg_usize, rhs_reg_usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().sshr(lhs_val, rhs_val))
                } else {
                    let l = emit_sext48(builder, lhs_val);
                    let r = emit_sext48(builder, rhs_val);
                    let result = builder.ins().sshr(l, r);
                    Ok(emit_tag_int(builder, result))
                }
            } else {
                call_helper(builder, helpers, "nulang_shr", &[lhs_val, rhs_val])
            }
        }
        BinOp::Pow => call_helper(builder, helpers, "nulang_pow", &[lhs_val, rhs_val]),
        BinOp::Assign => Err(AotCompileError::Unsupported(
            "BinOp::Assign is not a runtime operator — it should have been lowered away".into(),
        )),
        BinOp::Range => Err(AotCompileError::Unsupported("BinOp::Range".into())),
        BinOp::Pipe => Err(AotCompileError::Unsupported("BinOp::Pipe".into())),
    }
}

// ---------------------------------------------------------------------------
// Unary operation emission
// ---------------------------------------------------------------------------

fn compile_unary(
    builder: &mut FunctionBuilder,
    op: crate::ast::UnOp,
    operand: mir::LocalId,
    type_meta: &TypeMetadata,
    helpers: &HashMap<&str, FuncRef>,
    local_vals: &HashMap<u32, Value>,
    mode: CompileMode,
) -> AotResult<Value> {
    let reg = mir::FunctionBuilder::LOCAL_BASE + operand.0;
    let val = *local_vals
        .get(&reg)
        .ok_or_else(|| AotCompileError::Internal("uninitialized operand".into()))?;

    use crate::ast::UnOp;
    match op {
        UnOp::Neg => {
            if type_meta.is_known(reg as usize, KnownType::Int) {
                if mode == CompileMode::Unboxed {
                    Ok(builder.ins().ineg(val))
                } else {
                    let payload = emit_sext48(builder, val);
                    let neg = builder.ins().ineg(payload);
                    Ok(emit_tag_int(builder, neg))
                }
            } else if type_meta.is_known(reg as usize, KnownType::Float) {
                let f = builder.ins().bitcast(types::F64, MemFlags::new(), val);
                let neg = builder.ins().fneg(f);
                Ok(builder.ins().bitcast(types::I64, MemFlags::new(), neg))
            } else {
                call_helper(builder, helpers, "nulang_ineg", &[val])
            }
        }
        UnOp::Not => call_helper(builder, helpers, "nulang_not", &[val]),
        UnOp::Deref => Err(AotCompileError::Unsupported("UnOp::Deref".into())),
        UnOp::Ref(_) => Err(AotCompileError::Unsupported("UnOp::Ref".into())),
    }
}

/// Call a runtime helper function by name.
fn call_helper(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<&str, FuncRef>,
    name: &str,
    args: &[Value],
) -> AotResult<Value> {
    let func_ref = helpers
        .get(name)
        .copied()
        .ok_or_else(|| AotCompileError::Internal(format!("helper {} not registered", name)))?;
    let call = builder.ins().call(func_ref, args);
    Ok(builder.inst_results(call)[0])
}

/// Call a void-returning runtime helper function by name.
fn call_void_helper(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<&str, FuncRef>,
    name: &str,
    args: &[Value],
) -> AotResult<()> {
    let func_ref = helpers
        .get(name)
        .copied()
        .ok_or_else(|| AotCompileError::Internal(format!("void helper {} not registered", name)))?;
    builder.ins().call(func_ref, args);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ffi_call() {
        let mut builder = mir::FunctionBuilder::new("empty", None);
        builder.terminate(mir::Terminator::Return(None));
        let func = builder.build();
        assert!(func.type_metadata.is_empty());
        assert_eq!(func.name, "empty");
    }

    #[test]
    fn test_aot_compile_int_return() {
        let mut builder = mir::FunctionBuilder::new("answer", Some(crate::types::Type::int()));
        let tmp = builder.add_temp(crate::types::Type::int());
        builder.assign(tmp, mir::RValue::Const(crate::bytecode::Constant::Int(42)));
        builder.terminate(mir::Terminator::Return(Some(tmp)));
        let func = builder.build();
        let reg = mir::FunctionBuilder::LOCAL_BASE as usize + tmp.0 as usize;
        assert_eq!(func.type_metadata.get_type(reg), KnownType::Int);
    }

    #[test]
    fn test_aot_compile_add() {
        let mut builder = mir::FunctionBuilder::new("add", Some(crate::types::Type::int()));
        let a = builder.add_param("a", crate::types::Type::int());
        let b = builder.add_param("b", crate::types::Type::int());
        let sum = builder.add_temp(crate::types::Type::int());
        builder.assign(sum, mir::RValue::Binary(crate::ast::BinOp::Add, a, b));
        builder.terminate(mir::Terminator::Return(Some(sum)));
        let func = builder.build();
        let reg_a = mir::FunctionBuilder::LOCAL_BASE as usize + a.0 as usize;
        let reg_b = mir::FunctionBuilder::LOCAL_BASE as usize + b.0 as usize;
        let reg_sum = mir::FunctionBuilder::LOCAL_BASE as usize + sum.0 as usize;
        assert_eq!(func.type_metadata.get_type(reg_a), KnownType::Int);
        assert_eq!(func.type_metadata.get_type(reg_b), KnownType::Int);
        assert_eq!(func.type_metadata.get_type(reg_sum), KnownType::Int);
    }

    #[test]
    fn test_is_all_int_true() {
        let mut builder = mir::FunctionBuilder::new("all_int", Some(crate::types::Type::int()));
        builder.add_param("a", crate::types::Type::int());
        builder.add_param("b", crate::types::Type::int());
        let tmp = builder.add_temp(crate::types::Type::int());
        builder.assign(tmp, mir::RValue::Const(crate::bytecode::Constant::Int(7)));
        builder.terminate(mir::Terminator::Return(Some(tmp)));
        let func = builder.build();
        assert!(is_all_int(&func));
    }

    #[test]
    fn test_is_all_int_false_with_bool_param() {
        let mut builder = mir::FunctionBuilder::new("mixed", Some(crate::types::Type::int()));
        builder.add_param("x", crate::types::Type::int());
        builder.add_param("y", crate::types::Type::bool());
        let tmp = builder.add_temp(crate::types::Type::int());
        builder.assign(tmp, mir::RValue::Const(crate::bytecode::Constant::Int(1)));
        builder.terminate(mir::Terminator::Return(Some(tmp)));
        let func = builder.build();
        assert!(!is_all_int(&func));
    }
    #[test]
    fn test_is_all_int_false_with_void_return() {
        // void return is actually fine - is_all_int checks params only for non-Int,
        // and accepts None return. This tests a void function with non-Int param.
        let mut builder = mir::FunctionBuilder::new("void", None);
        builder.add_param("x", crate::types::Type::bool());
        builder.terminate(mir::Terminator::Return(None));
        let func = builder.build();
        assert!(!is_all_int(&func));
    }

    #[test]
    fn test_aot_compile_actor_state_access() {
        // Actor behaviors using StateGet/StateSet/SelfRef plus top-level
        // spawn+send must all compile under --backend native. Previously the
        // state ops and spawn were rejected with AOT Unsupported errors;
        // after wiring, the whole actor lifecycle (state access, spawn, send)
        // lowers without error.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Counter {
                state count: Int = 0
                behavior get() { self.count }
            }
            let c = spawn Counter {} in { send c get() }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module);
        let err = aot.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            err.is_empty(),
            "spawn + send + state access should all compile natively, got: {}",
            err
        );
    }

    #[test]
    fn test_aot_compile_and_invoke_behavior() {
        // A pure-compute actor behavior must lower to native code in the AOT
        // module's behavior table and be directly invocable through the
        // boxed calling convention, bypassing the bytecode VM.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Doubler {
                behavior double(x: Int) { x * 2 }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module)
            .expect("AOT compile of pure-compute behavior should succeed");
        let ptr = aot
            .fn_ptr_for_behavior("Doubler.double")
            .expect("behavior 'double' should be compiled");
        // Boxed calling convention: extern "C" fn(u64) -> u64.
        let f: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(ptr) };
        let result = f(crate::vm::Value::int(21).as_raw());
        let got = crate::vm::Value::from_bits(result).as_int();
        assert_eq!(got, Some(42));
    }

    #[test]
    fn test_aot_dispatch_actor_behavior() {
        // End-to-end native dispatch: a message routed through the
        // aot_behavior_adapter must run the AOT-compiled behavior body and
        // mutate the actor's durable state via the callback bridge.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Counter {
                state count: Int = 0
                behavior add(n: Int) { self.count = self.count + n }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module)
            .expect("AOT compile of stateful behavior should succeed");
        let native = aot
            .fn_ptr_for_behavior("Counter.add")
            .expect("behavior 'Counter.add' should be compiled");

        // Standalone actor with an `add` behavior dispatched through native code.
        let mut actor = crate::runtime::Actor::new(7, "Counter", 64);
        actor.set_state_field("count", crate::vm::Value::int(0));
        actor.register_behavior("add", crate::aot::aot_behavior_adapter);

        // Deliver `add(5)` then `add(7)`.
        crate::aot::set_aot_dispatch(Some(crate::aot::AotDispatchTarget::standalone(
            native, &aot,
        )));
        (actor.behavior_table[0].handler_fn)(&mut actor, &[crate::vm::Value::int(5)]);
        crate::aot::set_aot_dispatch(Some(crate::aot::AotDispatchTarget::standalone(
            native, &aot,
        )));
        (actor.behavior_table[0].handler_fn)(&mut actor, &[crate::vm::Value::int(7)]);

        let count = actor.get_state_field("count").and_then(|v| v.as_int());
        assert_eq!(
            count,
            Some(12),
            "AOT-native behavior should accumulate state"
        );
    }

    #[test]
    fn test_aot_native_send() {
        // Native message passing end-to-end: A's behavior executes a `send`
        // through the compiled `nulang_aot_send_N` helper, which delivers into
        // B's mailbox via the standalone registry; the driver then dispatches
        // the queued message through B's native behavior, mutating B's state.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor B {
                state count: Int = 0
                behavior add(n: Int) { self.count = self.count + n }
            }
            actor A {
                behavior fire(target, n: Int) { send target add(n) }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module)
            .expect("AOT compile of send behaviors should succeed");
        let fire = aot
            .fn_ptr_for_behavior("A.fire")
            .expect("behavior 'A.fire' should be compiled");
        let add = aot
            .fn_ptr_for_behavior("B.add")
            .expect("behavior 'B.add' should be compiled");

        let mut b = crate::runtime::Actor::new(2, "B", 64);
        b.set_state_field("count", crate::vm::Value::int(0));
        b.register_behavior("add", crate::aot::aot_behavior_adapter);
        let mut a = crate::runtime::Actor::new(1, "A", 64);
        a.register_behavior("fire", crate::aot::aot_behavior_adapter);
        crate::aot::register_aot_actor(&mut b);
        crate::aot::register_aot_actor(&mut a);

        // Dispatch A.fire(B_ref, 5): the native body sends `add(5)` to B.
        crate::aot::set_aot_dispatch(Some(crate::aot::AotDispatchTarget::standalone(fire, &aot)));
        (a.behavior_table[0].handler_fn)(
            &mut a,
            &[crate::vm::Value::actor_ref(2), crate::vm::Value::int(5)],
        );

        // B's mailbox must hold the queued message; dispatch it natively.
        let msg = b.mailbox.pop().expect("B should have received the message");
        assert_eq!(
            msg.behavior_id, 0,
            "add is B's first behavior (module index 0)"
        );
        crate::aot::set_aot_dispatch(Some(crate::aot::AotDispatchTarget::standalone(add, &aot)));
        (b.behavior_table[msg.behavior_id as usize].handler_fn)(&mut b, &msg.payload);

        let count = b.get_state_field("count").and_then(|v| v.as_int());
        assert_eq!(
            count,
            Some(5),
            "native send must deliver and mutate B's state"
        );

        crate::aot::unregister_aot_actor(2);
        crate::aot::unregister_aot_actor(1);
    }

    #[test]
    fn test_aot_native_receive() {
        // Selective receive in native code: Run() executes a `receive` that
        // pops a queued Add(5) from the actor's mailbox, binds n, and
        // accumulates it — all through AOT-compiled code + callbacks.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Counter {
                state total: Int = 0
                behavior Add(n: Int) { self.total = self.total + n }
                behavior Run() { receive { | Add(n) => self.total = self.total + n } }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module)
            .expect("AOT compile of receive behavior should succeed");
        let run = aot
            .fn_ptr_for_behavior("Counter.Run")
            .expect("behavior 'Counter.Run' should be compiled");

        let mut c = crate::runtime::Actor::new(1, "Counter", 64);
        c.set_state_field("total", crate::vm::Value::int(0));
        c.register_behavior("Add", crate::aot::aot_behavior_adapter);
        c.register_behavior("Run", crate::aot::aot_behavior_adapter);
        crate::aot::register_aot_actor(&mut c);

        // Queue an Add(5) message (behavior_id 0 = module index of Add).
        let _ = c.mailbox.push_local(crate::runtime::Message {
            behavior_id: 0,
            payload: std::sync::Arc::new(vec![crate::vm::Value::int(5)]),
            sender: 0,
            priority: crate::runtime::MessagePriority::Normal,
            trace_id: None,
        });

        // Dispatch Run(): its native body selectively receives the message.
        crate::aot::set_aot_dispatch(Some(crate::aot::AotDispatchTarget::standalone(run, &aot)));
        (c.behavior_table[1].handler_fn)(&mut c, &[]);

        let total = c.get_state_field("total").and_then(|v| v.as_int());
        assert_eq!(total, Some(5), "native receive must deliver the payload");

        crate::aot::unregister_aot_actor(1);
    }

    #[test]
    fn test_aot_native_spawn() {
        // Spawn in native code: Factory.make(base) runs `spawn Counter {
        // total = base }`, creating a fresh Counter with init state applied
        // and its behaviors registered — all through AOT-compiled code.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Counter {
                state total: Int = 0
                behavior Add(n: Int) { self.total = self.total + n }
                behavior GetTotal() { self.total }
            }
            actor Factory {
                behavior make(base: Int) { spawn Counter { total = base } }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module)
            .expect("AOT compile of spawn behavior should succeed");
        let make = aot
            .fn_ptr_for_behavior("Factory.make")
            .expect("behavior 'Factory.make' should be compiled");

        let mut factory = crate::runtime::Actor::new(10, "Factory", 64);
        factory.register_behavior("make", crate::aot::aot_behavior_adapter);
        crate::aot::register_aot_actor(&mut factory);

        let before = crate::aot::aot_actor_ids();
        crate::aot::set_aot_dispatch(Some(crate::aot::AotDispatchTarget::standalone(make, &aot)));
        crate::aot::set_aot_spawn_ctx(&aot);
        (factory.behavior_table[0].handler_fn)(&mut factory, &[crate::vm::Value::int(7)]);
        crate::aot::clear_aot_spawn_ctx();
        crate::aot::unregister_aot_actor(10);

        let after = crate::aot::aot_actor_ids();
        let spawned: Vec<u64> = after
            .into_iter()
            .filter(|id| !before.contains(id))
            .collect();
        assert_eq!(spawned.len(), 1, "spawn must create exactly one actor");
        let spawned_id = spawned[0];
        let c = crate::aot::aot_spawned_actor(spawned_id)
            .expect("spawned actor should be in the ownership registry");
        // SAFETY: the spawned actor is owned by the module registry.
        let c = unsafe { &mut *c };
        assert_eq!(
            c.name, "Counter",
            "spawned actor must have the right type name"
        );
        let total = c.get_state_field("total").and_then(|v| v.as_int());
        assert_eq!(total, Some(7), "spawn init must set total to base");
        let has_add = c.behavior_table.iter().any(|e| e.name == "Add");
        assert!(has_add, "spawned actor must register its behaviors");
        crate::aot::unregister_aot_actor(spawned_id);
    }

    #[test]
    fn test_aot_runtime_native_dispatch() {
        // Phase 3: the real actor `Runtime` dispatches a spawned actor's
        // behavior through AOT native code. A `Counter` actor spawned from a
        // CodeModule whose AotModule is registered must run `Add` natively
        // (handler = aot_behavior_adapter, target armed) and mutate state
        // through `AotRuntimeCallbacks` routing to the Runtime.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Counter {
                state total: Int = 0
                behavior Add(n: Int) { self.total = self.total + n }
                behavior Get() { self.total }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile should succeed");
        let code =
            crate::mir_codegen::compile_mir(&mut mir_module, "test").expect("bytecode compile");

        let mut rt = crate::runtime::Runtime::new();
        rt.register_aot_module(aot);
        let id = rt
            .spawn_from_module(&code, 0, Vec::new())
            .as_actor_id()
            .expect("spawn should return an actor ref");

        // The Add behavior must dispatch through the AOT adapter with an armed
        // target (proving native wiring, not bytecode).
        {
            let actor = rt.actors.get(&id).expect("spawned actor");
            assert_eq!(actor.behavior_table.len(), 2, "both behaviors registered");
            assert!(
                actor.behavior_table[0].handler_fn as usize
                    == crate::aot::aot_behavior_adapter
                        as fn(&mut crate::runtime::Actor, &[crate::vm::Value])
                        as usize,
                "Add should dispatch through the AOT adapter"
            );
            assert!(
                actor.aot_targets[0].is_some(),
                "Add should have an AOT dispatch target"
            );
        }

        // Deliver Add(5) through the scheduler: it must run the AOT-native body
        // and mutate the actor's state via the Runtime-routing callbacks.
        rt.send_message_by_id(id, 0, &[crate::vm::Value::int(5)]);
        rt.run_scheduler();

        let total = rt
            .actors
            .get(&id)
            .and_then(|a| a.get_state_field("total"))
            .and_then(|v| v.as_int());
        assert_eq!(
            total,
            Some(5),
            "AOT-native Add should mutate state through the Runtime"
        );
    }

    #[test]
    fn test_aot_runtime_native_spawn() {
        // Native spawn under the real Runtime: a Factory behavior runs
        // `spawn Counter { total = base }`, which must route through
        // Runtime::spawn_from_module so the new Counter is a live runtime actor
        // (registered, enqueued, AOT-wired) that then dispatches Add natively.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Counter {
                state total: Int = 0
                behavior Add(n: Int) { self.total = self.total + n }
            }
            actor Factory {
                behavior make(base: Int) { spawn Counter { total = base } }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let code =
            crate::mir_codegen::compile_mir(&mut mir_module, "test").expect("bytecode compile");

        let mut rt = crate::runtime::Runtime::new();
        rt.register_aot_module(aot);
        // Factory.make is the module's second behavior (index 1); Counter.Add
        // is index 0.
        let factory_id = rt
            .spawn_from_module(&code, 1, Vec::new())
            .as_actor_id()
            .expect("Factory spawn");
        let before: std::collections::HashSet<u64> = rt.actors.keys().copied().collect();

        // Dispatch Factory.make(7) natively -> spawns a Counter through the Runtime.
        rt.send_message_by_id(factory_id, 0, &[crate::vm::Value::int(7)]);
        rt.run_scheduler();

        let counter_id = *rt
            .actors
            .keys()
            .find(|id| !before.contains(id))
            .expect("make must spawn a new actor");
        {
            // The runtime names non-workflow actors `actor_{id}` (spawn.rs),
            // but the behavior must be AOT-wired and the init state applied.
            let counter = rt.actors.get(&counter_id).expect("spawned Counter");
            let total = counter.get_state_field("total").and_then(|v| v.as_int());
            assert_eq!(total, Some(7), "spawn init must set total to base");
            assert!(
                counter.behavior_table[0].handler_fn as usize
                    == crate::aot::aot_behavior_adapter
                        as fn(&mut crate::runtime::Actor, &[crate::vm::Value])
                        as usize,
                "spawned Counter's Add should be AOT-wired"
            );
            assert!(
                counter.aot_targets[0].is_some(),
                "spawned Counter's Add should have an AOT target"
            );
        }

        // The spawned Counter dispatches Add natively too.
        rt.send_message_by_id(counter_id, 0, &[crate::vm::Value::int(3)]);
        rt.run_scheduler();
        let total = rt
            .actors
            .get(&counter_id)
            .and_then(|a| a.get_state_field("total"))
            .and_then(|v| v.as_int());
        assert_eq!(
            total,
            Some(10),
            "spawned Counter should run Add natively through the Runtime"
        );
    }

    #[test]
    fn test_aot_runtime_native_effect() {
        // `perform Test.echo(n)` in an AOT-compiled behavior (no statically-
        // resolved handler) must route through the Runtime's builtin-effect
        // path: the installed test handler receives the arg and its return
        // value is written to the actor's state by the native body.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Store {
                state got: Int = 0
                behavior Set(n: Int) { self.got = perform Test.echo(n) }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let code =
            crate::mir_codegen::compile_mir(&mut mir_module, "test").expect("bytecode compile");

        let mut rt = crate::runtime::Runtime::new();
        rt.register_aot_module(aot);
        // Mock the builtin: Test.echo returns double its integer arg. This is
        // intercepted by `check_test_handler` before real dispatch, so it
        // proves the AOT perform routed through the Runtime builtin path.
        rt.install_test_handler("Test.echo", |regs| {
            regs.get(0)
                .and_then(|v| v.as_int())
                .map(|n| crate::vm::Value::int(n * 2))
        });

        // Set is the module's only behavior (index 0).
        let id = rt
            .spawn_from_module(&code, 0, Vec::new())
            .as_actor_id()
            .expect("Store spawn");
        assert!(
            rt.actors.get(&id).unwrap().aot_targets[0].is_some(),
            "Set should be AOT-wired"
        );

        rt.send_message_by_id(id, 0, &[crate::vm::Value::int(7)]);
        rt.run_scheduler();

        let got = rt
            .actors
            .get(&id)
            .and_then(|a| a.get_state_field("got"))
            .and_then(|v| v.as_int());
        assert_eq!(
            got,
            Some(14),
            "AOT-native perform must route through the Runtime test handler"
        );
    }

    #[test]
    fn test_aot_runtime_native_emit() {
        // `emit Incremented(n)` in an AOT-compiled behavior must route through
        // the Runtime's `emit_event`, recording the event on `actor.event_log`
        // exactly as the bytecode Emit opcode does.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Counter {
                behavior inc(n: Int) { emit Incremented(n) }
            }
            fn main() { 0 }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let code =
            crate::mir_codegen::compile_mir(&mut mir_module, "test").expect("bytecode compile");

        let mut rt = crate::runtime::Runtime::new();
        rt.register_aot_module(aot);
        let id = rt
            .spawn_from_module(&code, 0, Vec::new())
            .as_actor_id()
            .expect("Counter spawn");
        assert!(
            rt.actors.get(&id).unwrap().aot_targets[0].is_some(),
            "inc should be AOT-wired"
        );

        rt.send_message_by_id(id, 0, &[crate::vm::Value::int(7)]);
        rt.run_scheduler();

        let log = &rt.actors.get(&id).expect("actor").event_log;
        assert_eq!(log.len(), 1, "one event should be emitted");
        assert_eq!(log[0].0, "Incremented", "event name should match");
        assert_eq!(
            log[0].1.first().and_then(|v| v.as_int()),
            Some(7),
            "event arg should be delivered"
        );
    }

    #[test]
    fn test_aot_abortive_handle_body_no_perform() {
        // An abortive (non-resuming) `handle` whose body never `perform`s the
        // handled effect compiles in AOT: the handler body block is unreachable
        // and the handle value is the body value. This documents the current
        // baseline before PerformDirect (perform → handler body) support.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"handle { 5 } { | IO.err() => 0 }"#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(5),
            "abortive handle without a perform evaluates to the body value"
        );
    }

    #[test]
    fn test_aot_resuming_handler_continuation() {
        // A resuming handler: `perform Rand.int()` jumps to the handler body,
        // which resumes with 41, and control returns to the point after the
        // perform with `a` = 41, so `a + 1` = 42. Compiled entirely as native
        // intra-function control flow (no stack capture).
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            fn f() -> Int {
                handle {
                    let a = perform Rand.int()
                    a + 1
                } { | Rand.int() resume => 41 }
            }
            fn main() { f() }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(42),
            "resuming handler must deliver 41 then continue with a+1"
        );
    }

    #[test]
    fn test_aot_resuming_handler_with_param() {
        // A resuming handler with a parameter: `perform Echo.run(5)` passes 5
        // into the handler body's `x` (a handler-body block param), which
        // resumes with x + 1 = 6; control returns with `a` = 6.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            effect Echo { run: Int -> Int }
            fn f() -> Int {
                handle {
                    let a = perform Echo.run(5)
                    a * 2
                } { | Echo.run(x) resume => x + 1 }
            }
            fn main() { f() }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(12),
            "handler param x must receive 5, resume 6, then a*2 = 12"
        );
    }

    #[test]
    fn test_aot_abortive_handler_with_param() {
        // An abortive handler (no `resume`): `perform CustomErr.raise(7)`
        // transfers control to the handler body, which receives x = 7 and
        // computes 7 + 100 = 107. The post-perform `a * 2` is dead (control
        // never returns from an abortive perform).
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            effect CustomErr { raise: Int -> Int }
            fn f() -> Int {
                handle {
                    let a = perform CustomErr.raise(7)
                    a * 2
                } { | CustomErr.raise(x) => x + 100 }
            }
            fn main() { f() }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(107),
            "abortive handler must receive x = 7 and yield 7 + 100 = 107 (post-perform code is dead)"
        );
    }

    #[test]
    fn test_aot_abortive_handler_no_param() {
        // Abortive handler with no effect argument: the handle body's literal
        // is bypassed, control goes to the handler body.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            effect Boom { go: -> Int }
            fn f() -> Int {
                handle {
                    let a = perform Boom.go()
                    a + 1
                } { | Boom.go() => 99 }
            }
            fn main() { f() }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(99),
            "abortive handler with no args must yield the handler body result"
        );
    }

    #[test]
    fn test_aot_resuming_handler_multi_perform() {
        // Two performs of the SAME resuming handler in one handle body. Each
        // perform gets its own continuation; the handler body's Resume
        // dispatches on the continuation-index block param.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            effect Echo { run: Int -> Int }
            fn f() -> Int {
                handle {
                    let a = perform Echo.run(1)
                    let b = perform Echo.run(2)
                    a + b
                } { | Echo.run(x) resume => x + 10 }
            }
            fn main() { f() }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(23),
            "a = 1+10 = 11, b = 2+10 = 12, a+b = 23"
        );
    }

    fn aot_compile_source(source: &str) -> crate::aot::AotModule {
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        crate::aot::AotModule::compile(&mir_module).expect("AOT compile")
    }

    #[test]
    fn test_aot_resuming_handler_multi_block_if_else() {
        // Same resuming handler performed from the THEN and ELSE branches of an
        // `if` — i.e. from two DIFFERENT MIR blocks on exclusive paths. Before
        // the uniform-threaded-width change this was rejected. Each branch's
        // continuation reads only its own perform result (no cross-block read).
        let aot = aot_compile_source(
            r#"
            effect Echo { run: Int -> Int }
            fn f(cond: Bool) -> Int {
                handle {
                    if cond then { perform Echo.run(1) } else { perform Echo.run(2) }
                } { | Echo.run(x) resume => x + 10 }
            }
            fn main() { f(true) + f(false) }
            "#,
        );
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(23),
            "f(true)=1+10=11, f(false)=2+10=12, sum=23"
        );
    }

    #[test]
    fn test_aot_resuming_handler_multi_block_discarded_first() {
        // A resuming perform whose result is discarded (the `if` statement
        // value is unused), followed by a second perform of the same handler in
        // a later block. The first site's dst is a dead merge local, not a
        // cross-block read, so this must compile (not be guarded off).
        let aot = aot_compile_source(
            r#"
            effect Echo { run: Int -> Int }
            fn f(cond: Bool) -> Int {
                handle {
                    if cond then { perform Echo.run(1) }
                    let b = perform Echo.run(2)
                    b
                } { | Echo.run(x) resume => x + 10 }
            }
            fn main() { f(true) + f(false) }
            "#,
        );
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(24),
            "f(true)=2+10=12, f(false)=2+10=12, sum=24"
        );
    }

    #[test]
    fn test_aot_array_access_boxed_entry() {
        // Array indexing at the top level (a BOXED entry function) previously
        // returned nil: the boxed ArrayLoad passed the TAGGED index to
        // `nulang_obj_get`, which used it raw → out of range. Masking the
        // payload fixes both boxed and unboxed paths.
        let aot = aot_compile_source(
            r#"
            fn f() -> Int { let a = [1, 2, 3] in a[1] }
            fn main() { f() }
            "#,
        );
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(2),
            "array indexing must work"
        );
    }

    #[test]
    fn test_aot_float_pow_and_int_pow_overflow() {
        // `3.14 ** 2.0` (float pow) must equal the interpreter's `powf`
        // result, not the int-only 1. And int pow overflow must WRAP (0),
        // matching the interpreter's `wrapping_mul`, not return nil.
        let aot = aot_compile_source(
            r#"
            fn main() -> Float { 3.14 ** 2.0 }
            "#,
        );
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_float(),
            Some(9.8596),
            "3.14 ** 2.0 must be a float pow"
        );

        let aot = aot_compile_source(r#"fn main() -> Int { 1000000000 ** 1000000000 }"#);
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(0),
            "int pow overflow wraps (wrapping_mul), not nil"
        );
    }

    #[test]
    fn test_aot_int_pow_non_overflow_and_negative_exp() {
        // Non-overflow int pow must produce the exact value — this exercised
        // the unboxed-compilation hazard: `Pow` was missing from the
        // nil-producing exclusion in `is_all_int`, so an all-Int function
        // compiled unboxed, fell through the unboxed binop match (no Pow arm)
        // to `call_helper(nulang_pow)` with RAW operands, which were misread
        // as floats (1.0), whose zero payload re-tagged as int 0. `3 ** 3`
        // returned 0 instead of 27. Pow is now excluded from unboxed mode.
        let aot = aot_compile_source(r#"fn main() -> Int { 3 ** 3 }"#);
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(27),
            "3 ** 3 must be 27, not 0 (unboxed-pow hazard)"
        );

        // Binary-exponentiation path: 2 ** 10 = 1024.
        let aot = aot_compile_source(r#"fn main() -> Int { 2 ** 10 }"#);
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(1024),
            "2 ** 10 must be 1024"
        );

        // Negative exponent yields nil (mirrors IDiv div-by-zero) — boxed
        // path must surface nil, not re-tag it as int 0.
        let aot = aot_compile_source(r#"fn main() -> Int { 3 ** -1 }"#);
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw),
            crate::vm::Value::nil(),
            "3 ** -1 must be nil, not int 0"
        );
    }

    #[test]
    fn test_aot_unary_neg_of_computed_float() {
        // `-(0.1 + 0.22)` previously mis-compiled: `binary_type` typed the
        // float add result as Int (HIR vars carry Type::unit()), so the MIR
        // local metadata marked it Int and `Unary Neg` took the int path —
        // negating the float bits as a payload → garbage. Both the literal
        // case and the through-a-var case must match the interpreter (-0.32).
        let aot = aot_compile_source(r#"fn main() -> Float { -(0.1 + 0.22) }"#);
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_float(),
            Some(-0.32),
            "neg of literal float sum"
        );

        let aot =
            aot_compile_source(r#"fn main() -> Float { let x = 0.1; let y = 0.2; -(x + y) }"#);
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_float(),
            Some(-0.30000000000000004),
            "neg of float sum through vars"
        );
    }

    #[test]
    fn test_aot_div_by_zero_returns_nil() {
        // Integer division/modulo by zero must return nil (matching the
        // interpreter), not trap. The AOT previously computed `sdiv`/`srem`
        // unconditionally and `select`-ed nil AFTER — so a zero divisor
        // trapped (SIGILL) before the select. Now routed through the
        // zero-checking helper. Exercised both from a function (unboxed) and
        // the top level (boxed entry).
        let aot = aot_compile_source(r#"fn main() { 1 / 0 }"#);
        let raw = aot.run().expect("native run");
        assert!(crate::vm::Value::from_raw(raw).is_nil(), "1/0 must be nil");
        let aot = aot_compile_source(r#"fn f() -> Int { 1 % 0 } fn main() { f() }"#);
        let raw = aot.run().expect("native run");
        assert!(crate::vm::Value::from_raw(raw).is_nil(), "1%0 must be nil");
    }

    #[test]
    fn test_aot_mutable_var_assigned_in_branch_read_after_join() {
        // A `var` assigned in ONE branch and read after the merge point is a
        // general SSA/phi-placement case: it gets a different value on each
        // incoming path (branch def vs. the flowed-through prior value), so the
        // join needs a block param. Previously the intersection-of-predecessor-
        // defs heuristic missed it → the value was referenced from a non-
        // dominating block → CLIF verifier error. `f(true)=6, f(false)=5`.
        let aot = aot_compile_source(
            r#"
            fn f(cond: Bool) -> Int {
                var acc = 0
                if cond then { acc = 1 }
                acc + 5
            }
            fn main() { f(true) + f(false) }
            "#,
        );
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(11),
            "f(true)=1+5=6, f(false)=0+5=5, sum=11"
        );
    }

    #[test]
    fn test_aot_resuming_handler_multi_block_cross_read() {
        // A resuming perform's result stored to a mutable `acc` in one block
        // and READ after a second perform of the same handler in a later block.
        // The continuation of the later perform is not dominated by its block
        // (the shared handler body makes it reachable from the earlier block
        // too), so the cross-block value must be threaded through the handler's
        // Resume dispatch as a continuation live-in (Phase 2).
        // f(true)=acc=11, b=12, acc+b=23; f(false)=acc=0, b=12, acc+b=12.
        let aot = aot_compile_source(
            r#"
            effect Echo { run: Int -> Int }
            fn f(cond: Bool) -> Int {
                handle {
                    var acc = 0
                    if cond then { acc = perform Echo.run(1) }
                    let b = perform Echo.run(2)
                    acc + b
                } { | Echo.run(x) resume => x + 10 }
            }
            fn main() { f(true) + f(false) }
            "#,
        );
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(35),
            "f(true)=11+12=23, f(false)=0+12=12, sum=35"
        );
    }

    #[test]
    fn test_aot_resuming_handler_loop_carried() {
        // A resuming perform in a loop body whose result feeds the next
        // iteration (loop-carried accumulator). The continuation reads the
        // accumulator (a loop merge block param), which is a continuation
        // live-in that must thread through the handler even for a SINGLE
        // perform site. acc: 0 -> 1 -> 2 -> 3.
        let aot = aot_compile_source(
            r#"
            effect Gen { next: Int -> Int }
            fn f(n: Int) -> Int {
                handle {
                    var acc = 0
                    var i = 0
                    while i < n {
                        acc = perform Gen.next(acc)
                        i = i + 1
                    }
                    acc
                } { | Gen.next(x) resume => x + 1 }
            }
            fn main() { f(3) }
            "#,
        );
        let raw = aot.run().expect("native run");
        assert_eq!(
            crate::vm::Value::from_raw(raw).as_int(),
            Some(3),
            "loop: acc 0->1->2->3 = 3"
        );
    }

    #[test]
    fn test_aot_runtime_native_ask() {
        // `ask target behavior(args)` in an AOT-compiled behavior must compile
        // and route through the Runtime's synchronous ask path
        // (`ask_actor_sync`), the same path the bytecode `Ask` opcode takes.
        // The native (AOT) ask contract returns nil for an AOT-wired target —
        // the runtime's native-handler path is fire-and-forget and does not
        // capture a return — so the behavior completes and the helper runs;
        // we assert the ask result follows that contract.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor Svc {
                behavior get(n: Int) -> Int { n * 10 }
            }
            actor Caller {
                behavior go(n: Int) {
                    let svc = spawn Svc {}
                    let v = ask svc get(n)
                    emit Got(v)
                }
            }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let code =
            crate::mir_codegen::compile_mir(&mut mir_module, "test").expect("bytecode compile");

        let mut rt = crate::runtime::Runtime::new();
        rt.register_aot_module(aot);
        // Behavior index 1 = Caller.go (Svc.get is 0).
        let caller = rt
            .spawn_from_module(&code, 1, Vec::new())
            .as_actor_id()
            .expect("Caller spawn");

        rt.send_message_by_id(caller, 0, &[crate::vm::Value::int(5)]);
        rt.run_scheduler();

        let log = &rt.actors.get(&caller).expect("caller actor").event_log;
        assert_eq!(log.len(), 1, "one event should be emitted");
        assert_eq!(log[0].0, "Got", "event name should match");
        // Native-ask contract: an AOT-wired target's ask returns nil.
        assert_eq!(
            log[0].1.first(),
            Some(&crate::vm::Value::nil()),
            "AOT ask on an AOT-wired target follows the native fire-and-forget contract (nil)"
        );
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    #[cfg(feature = "ai-runtime")]
    fn test_aot_runtime_native_perform_async() {
        // `perform Pipeline.new()` in an AOT-compiled behavior must compile
        // and route through the Runtime's `perform_async` path (the same one
        // the bytecode PerformAsync opcode takes). Pipeline.new completes
        // synchronously with a string id, which the helper materializes as a
        // heap string; the behavior emits it as an event arg.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor W {
                behavior go() {
                    let pid = Pipeline.new()
                    emit Got(pid)
                }
            }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let code =
            crate::mir_codegen::compile_mir(&mut mir_module, "test").expect("bytecode compile");

        let mut rt = crate::runtime::Runtime::new();
        rt.register_aot_module(aot);
        // Behavior index 0 = W.go.
        let w = rt
            .spawn_from_module(&code, 0, Vec::new())
            .as_actor_id()
            .expect("W spawn");

        rt.send_message_by_id(w, 0, &[]);
        rt.run_scheduler();

        let log = &rt.actors.get(&w).expect("W actor").event_log;
        assert_eq!(log.len(), 1, "one event should be emitted");
        assert_eq!(log[0].0, "Got", "event name should match");
        let pid = log[0]
            .1
            .first()
            .expect("event should carry the pipeline id");
        assert!(
            pid.is_string() || pid.as_ptr().is_some(),
            "pipeline id must be a string value, got {:?}",
            pid
        );
    }

    #[test]
    fn test_aot_runtime_native_signal_wait() {
        // `perform Signal.wait("sig")` in an AOT-compiled behavior must
        // compile and route through the callbacks' `wait_signal` (the same
        // path the bytecode SignalWait opcode takes). Outside a workflow the
        // default callback delivers unit, which the behavior emits.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor W {
                behavior go() {
                    let s = perform Signal.wait("sig")
                    emit Got(s)
                }
            }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let code =
            crate::mir_codegen::compile_mir(&mut mir_module, "test").expect("bytecode compile");

        let mut rt = crate::runtime::Runtime::new();
        rt.register_aot_module(aot);
        // Behavior index 0 = W.go.
        let w = rt
            .spawn_from_module(&code, 0, Vec::new())
            .as_actor_id()
            .expect("W spawn");

        rt.send_message_by_id(w, 0, &[]);
        rt.run_scheduler();

        let log = &rt.actors.get(&w).expect("W actor").event_log;
        assert_eq!(log.len(), 1, "one event should be emitted");
        assert_eq!(log[0].0, "Got", "event name should match");
        assert_eq!(
            log[0].1.first(),
            Some(&crate::vm::Value::unit()),
            "Signal.wait outside a workflow delivers unit"
        );
    }

    #[test]
    fn test_aot_runtime_native_migrate() {
        // `migrate actor to node` in an AOT-compiled behavior must compile and
        // deliver unit (the native backend has no distribution layer, matching
        // the bytecode VM's no-distributed-callbacks contract).
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            actor W {
                behavior go() {
                    let s = migrate self to 99
                    emit Got(s)
                }
            }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let code =
            crate::mir_codegen::compile_mir(&mut mir_module, "test").expect("bytecode compile");

        let mut rt = crate::runtime::Runtime::new();
        rt.register_aot_module(aot);
        // Behavior index 0 = W.go.
        let w = rt
            .spawn_from_module(&code, 0, Vec::new())
            .as_actor_id()
            .expect("W spawn");

        rt.send_message_by_id(w, 0, &[]);
        rt.run_scheduler();

        let log = &rt.actors.get(&w).expect("W actor").event_log;
        assert_eq!(log.len(), 1, "one event should be emitted");
        assert_eq!(log[0].0, "Got", "event name should match");
        assert_eq!(
            log[0].1.first(),
            Some(&crate::vm::Value::unit()),
            "migrate without a distribution layer delivers unit"
        );
    }

    #[test]
    fn test_aot_no_entry_function_returns_nil() {
        // A module with only a function definition (no top-level expression)
        // has no entry; running it must yield nil, not call a parameterized
        // function with no args (which previously returned garbage).
        let aot = aot_compile_source(r#"fn mix(x, y) { x + y * 2 }"#);
        let raw = aot.run().expect("native run");
        assert!(
            crate::vm::Value::from_raw(raw).is_nil(),
            "library module must run to nil"
        );
    }

    #[test]
    fn test_aot_capability_check_is_true() {
        // Capability checks are compile-time only; the AOT backend must
        // compile `CapabilityCheck` to tagged true (the bytecode backend
        // emits Const1). No source syntax currently produces CapCheck, so
        // this is exercised at the MIR level.
        let mut builder = mir::FunctionBuilder::new("main", Some(crate::types::Type::bool()));
        let tmp = builder.add_temp(crate::types::Type::int());
        let out = builder.add_temp(crate::types::Type::bool());
        builder.assign(tmp, mir::RValue::Const(crate::bytecode::Constant::Int(1)));
        builder.assign(out, mir::RValue::CapabilityCheck { val: tmp });
        builder.terminate(mir::Terminator::Return(Some(out)));
        let func = builder.build();
        let module = mir::Module {
            name: "capcheck".into(),
            functions: vec![func],
            behaviors: vec![],
            actor_metadata: vec![],
            compensation_of: vec![],
            parallel_branches_of: vec![],
            foreign_functions: vec![],
        };
        let aot = crate::aot::AotModule::compile(&module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        let val = crate::vm::Value::from_raw(raw);
        assert_eq!(
            val,
            crate::vm::Value::bool(true),
            "CapabilityCheck must yield true"
        );
    }

    #[test]
    fn test_aot_ffi_call() {
        // `extern { fn double_int(x: Int) -> Int }` invoked from AOT-compiled
        // code must resolve the pre-registered native function through the
        // global FFI registry (the same resolve_or_load + call_native path the
        // bytecode FFICall opcode uses) and deliver its result.
        extern "C" fn double_int(x: i64) -> i64 {
            x * 2
        }
        let sig = crate::ffi::marshal::Signature::new(
            vec![crate::ffi::marshal::CType::I64],
            crate::ffi::marshal::CType::I64,
        );
        // SAFETY: double_int's ABI matches the declared signature.
        unsafe {
            crate::ffi::native::register_native_function(
                "double_int",
                double_int as *const core::ffi::c_void,
                sig,
            )
            .expect("register");
        }

        let mut builder = mir::FunctionBuilder::new("main", Some(crate::types::Type::int()));
        let arg = builder.add_temp(crate::types::Type::int());
        let out = builder.add_temp(crate::types::Type::int());
        builder.assign(arg, mir::RValue::Const(crate::bytecode::Constant::Int(21)));
        builder.assign(
            out,
            mir::RValue::FFICall {
                idx: 0,
                args: vec![arg],
            },
        );
        builder.terminate(mir::Terminator::Return(Some(out)));
        let func = builder.build();
        let module = mir::Module {
            name: "ffi".into(),
            functions: vec![func],
            behaviors: vec![],
            actor_metadata: vec![],
            compensation_of: vec![],
            parallel_branches_of: vec![],
            foreign_functions: vec![mir::ForeignFunction {
                library: "".into(), // pre-registered under (None, "double_int")
                symbol: "double_int".into(),
                params: vec![crate::types::Type::Primitive(
                    crate::types::PrimitiveType::Int,
                )],
                ret: crate::types::Type::Primitive(crate::types::PrimitiveType::Int),
            }],
        };
        let aot = crate::aot::AotModule::compile(&module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        let val = crate::vm::Value::from_raw(raw);
        assert_eq!(
            val.as_int(),
            Some(42),
            "FFI double_int(21) should deliver 21 * 2 = 42"
        );
    }

    #[test]
    fn test_aot_closure_capture() {
        // A closure capturing an enclosing local must allocate a closure
        // object carrying the capture, and a call through it must dispatch to
        // the lifted target with (explicit args + captures) — mirroring the
        // bytecode Closure/CapStore/CapLoad/ClosureCall path.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = "let a = 40 in let add = fn(x) { x + a } in add(2)";
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        let val = crate::vm::Value::from_raw(raw);
        assert_eq!(
            val.as_int(),
            Some(42),
            "closure capturing a=40 applied to 2 should yield 42"
        );
    }

    #[test]
    fn test_aot_closure_capture_two_vars() {
        // Two captured locals exercise the multi-capture make/call helpers
        // (closure object with two capture slots; total call arity 3).
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = "let a = 30 in let b = 10 in let f = fn(x) { a + b + x } in f(2)";
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        let val = crate::vm::Value::from_raw(raw);
        assert_eq!(
            val.as_int(),
            Some(42),
            "closure capturing a=30, b=10 applied to 2 should yield 42"
        );
    }

    #[test]
    fn test_aot_recursive_function() {
        // A recursive function (factorial) must compile and execute correctly
        // in the AOT backend. The unboxed variant must handle self-recursive
        // calls and arithmetic on the call result.
        use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        let source = r#"
            fn fact(n: Int) -> Int {
                if n <= 1 then 1 else n * fact(n - 1)
            }
            fn main() { fact(5) }
        "#;
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let mut ec = EffectChecker::new();
        ec.check_module(&ast.decls).unwrap();
        let mut ca = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        for d in crate::effect_checker::flatten_decls(&ast.decls) {
            if let crate::ast::Decl::Function { body, .. } = d {
                ca.infer_cap(&ctx, body).unwrap();
            }
        }
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir_module = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = crate::aot::AotModule::compile(&mir_module).expect("AOT compile");
        let raw = aot.run().expect("native run");
        let val = crate::vm::Value::from_raw(raw);
        assert_eq!(
            val.as_int(),
            Some(120),
            "recursive fact(5) should yield 120"
        );
    }

    #[test]
    fn test_is_all_int_empty() {
        let mut builder = mir::FunctionBuilder::new("empty_int", Some(crate::types::Type::int()));
        let tmp = builder.add_temp(crate::types::Type::int());
        builder.assign(tmp, mir::RValue::Const(crate::bytecode::Constant::Int(0)));
        builder.terminate(mir::Terminator::Return(Some(tmp)));
        let func = builder.build();
        assert!(is_all_int(&func));
    }
}
