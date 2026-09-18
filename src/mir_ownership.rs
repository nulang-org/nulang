//! Conservative MIR ownership-transfer inference.
//!
//! This pass recovers ownership flow through plain local copies without
//! changing source-language semantics. It intentionally starts narrow:
//!
//! - source must be a compiler temporary (anonymous or `__*`);
//! - source must have one owning definition;
//! - source must have exactly one read in the whole function;
//! - that read must be `dst = Load(src)`;
//! - params/captures/handler params are excluded;
//! - only pointer-capable types are considered.
//!
//! The rewrite preserves the backend-neutral runtime representation used by
//! explicit `consume`: `dst = Load(src); src = nil`, and records the same
//! `OwnershipTransfer` metadata consumed by Drop planning.

use crate::bytecode::Constant;
use crate::mir::{self, LocalId};
use crate::types::{Capability, PrimitiveType, Type};
use rustc_hash::{FxHashMap, FxHashSet};

/// Infer safe last-use ownership transfers in every function and behavior.
/// Returns the number of newly inferred transfer edges.
pub fn infer_last_use_transfers(module: &mut mir::Module) -> usize {
    let mut total = 0;
    for func in &mut module.functions {
        total += infer_function_fixed_point(func);
    }
    for func in &mut module.behaviors {
        total += infer_function_fixed_point(func);
    }
    total
}

fn infer_function_fixed_point(func: &mut mir::Function) -> usize {
    let mut total = 0;
    // Each successful round creates at least one new transfer edge, and a
    // given source cannot be inferred twice. The local count is therefore a
    // hard termination bound even for copy chains.
    for _ in 0..func.locals.len().max(1) {
        let n = infer_function_round(func);
        total += n;
        if n == 0 {
            break;
        }
    }
    total
}

fn infer_function_round(func: &mut mir::Function) -> usize {
    let nlocals = func.locals.len();
    if nlocals == 0 {
        return 0;
    }

    let transfers: FxHashSet<(LocalId, LocalId)> = func
        .ownership_transfers
        .iter()
        .map(|t| (t.src, t.dst))
        .collect();

    let mut external = FxHashSet::default();
    external.extend(func.params.iter().copied());
    external.extend(func.captures.iter().copied());
    for table in &func.handler_tables {
        for binding in &table.bindings {
            external.extend(binding.params.iter().copied());
        }
    }

    let mut def_count = vec![0usize; nlocals];
    let mut owning_def = vec![false; nlocals];
    let mut use_count = vec![0usize; nlocals];
    let mut load_site: Vec<Option<(usize, usize, LocalId)>> = vec![None; nlocals];

    for (bi, block) in func.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            if let mir::Stmt::Assign { dst, op } = stmt {
                let d = dst.0 as usize;
                def_count[d] += 1;
                if def_count[d] == 1 {
                    owning_def[d] = rvalue_is_owning(op)
                        || matches!(
                            op,
                            mir::RValue::Load(src)
                                if transfers.contains(&(*src, *dst))
                        );
                } else {
                    owning_def[d] = false;
                }

                if let mir::RValue::Load(src) = op
                    && *src != *dst
                {
                    load_site[src.0 as usize] = Some((bi, si, *dst));
                }
            }

            let mut reads = Vec::new();
            stmt_reads(stmt, &mut reads);
            for id in reads {
                use_count[id.0 as usize] += 1;
            }
        }

        let mut reads = Vec::new();
        terminator_reads(&block.terminator, &mut reads);
        for id in reads {
            use_count[id.0 as usize] += 1;
        }
    }

    let mut inferred: Vec<(usize, usize, LocalId, LocalId)> = Vec::new();
    for i in 0..nlocals {
        let src = LocalId(i as u32);
        let local = &func.locals[i];

        let compiler_temp = local
            .name
            .as_deref()
            .map(|n| n.starts_with("__"))
            .unwrap_or(true);
        if !compiler_temp
            || external.contains(&src)
            || !may_hold_heap_ptr(&local.ty)
            || def_count[i] != 1
            || !owning_def[i]
            || use_count[i] != 1
        {
            continue;
        }

        let Some((bi, si, dst)) = load_site[i] else {
            continue;
        };
        if src == dst
            || external.contains(&dst)
            || def_count[dst.0 as usize] != 1
            || transfers.contains(&(src, dst))
        {
            continue;
        }

        // The unique read guarantee means this Load is the source's last
        // semantic observation on every path represented by this MIR local.
        inferred.push((bi, si, src, dst));
    }

    if inferred.is_empty() {
        return 0;
    }

    let mut by_site: FxHashMap<(usize, usize), (LocalId, LocalId)> = FxHashMap::default();
    for &(bi, si, src, dst) in &inferred {
        by_site.insert((bi, si), (src, dst));
        func.ownership_transfers
            .push(mir::OwnershipTransfer { src, dst });
    }

    let mut insertions: FxHashMap<mir::BlockId, Vec<usize>> = FxHashMap::default();
    for (bi, block) in func.blocks.iter_mut().enumerate() {
        let old = std::mem::take(&mut block.stmts);
        let mut rewritten = Vec::with_capacity(old.len() + inferred.len());
        for (si, stmt) in old.into_iter().enumerate() {
            rewritten.push(stmt);
            if let Some((src, _dst)) = by_site.get(&(bi, si)).copied() {
                rewritten.push(mir::Stmt::Assign {
                    dst: src,
                    op: mir::RValue::Const(Constant::Nil),
                });
                insertions.entry(block.id).or_default().push(si);
            }
        }
        block.stmts = rewritten;
    }

    // Synthetic clear statements have no source line of their own. Shift the
    // existing line table so all original statements still point at the same
    // source locations after insertion.
    if !insertions.is_empty() {
        for ((bid, si), _) in &mut func.line_table {
            if let Some(points) = insertions.get(bid) {
                let shift = points.iter().filter(|&&p| p < *si).count();
                *si += shift;
            }
        }
    }

    inferred.len()
}

fn may_hold_heap_ptr(ty: &Type) -> bool {
    match ty {
        Type::Primitive(p) => matches!(p, PrimitiveType::String | PrimitiveType::Unit),
        Type::Tuple(_)
        | Type::Record(_)
        | Type::Array(_)
        | Type::App { .. }
        | Type::Var(_)
        | Type::Variant(_)
        | Type::Function { .. }
        | Type::Actor { .. }
        | Type::Scheme { .. }
        | Type::Reference { .. } => true,
        Type::Skolem(_) => false,
        Type::Nominal { underlying, .. } => may_hold_heap_ptr(underlying),
    }
}

fn rvalue_is_owning(op: &mir::RValue) -> bool {
    matches!(
        op,
        mir::RValue::Tuple(_)
            | mir::RValue::Record(_)
            | mir::RValue::RecordUpdate { .. }
            | mir::RValue::ArrayLit(_)
            | mir::RValue::Const(_)
    )
}

fn stmt_reads(stmt: &mir::Stmt, out: &mut Vec<LocalId>) {
    match stmt {
        mir::Stmt::Assign { op, .. } => rvalue_reads(op, out),
        mir::Stmt::StoreFieldNamed { obj, src, .. } => {
            out.push(*obj);
            out.push(*src);
        }
        mir::Stmt::ArrayStore { arr, idx, src } => {
            out.push(*arr);
            out.push(*idx);
            out.push(*src);
        }
        mir::Stmt::Emit { args, .. } => out.extend(args.iter().copied()),
        mir::Stmt::StateSet { src, .. } => out.push(*src),
        mir::Stmt::EnterHandle { .. } | mir::Stmt::PopHandler => {}
    }
}

fn terminator_reads(term: &mir::Terminator, out: &mut Vec<LocalId>) {
    match term {
        mir::Terminator::Return(Some(id)) | mir::Terminator::Resume(id) => out.push(*id),
        mir::Terminator::Branch { cond, .. } => out.push(*cond),
        mir::Terminator::Return(None)
        | mir::Terminator::Jump(_)
        | mir::Terminator::Unterminated => {}
    }
}

fn rvalue_reads(rv: &mir::RValue, out: &mut Vec<LocalId>) {
    use mir::RValue;
    match rv {
        RValue::Const(_)
        | RValue::SignalWait { .. }
        | RValue::Receive
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveCommit
        | RValue::SelfRef
        | RValue::Panic(_)
        | RValue::StateGet { .. } => {}
        RValue::Load(x)
        | RValue::ArrayLen(x)
        | RValue::Unary(_, x)
        | RValue::Resume(x)
        | RValue::CapabilityCheck { val: x } => out.push(*x),
        RValue::LoadFieldNamed { obj, .. } | RValue::LoadFieldPos { obj, .. } => out.push(*obj),
        RValue::ArrayLoad { arr, idx }
        | RValue::Binary(_, arr, idx)
        | RValue::StringEq(arr, idx)
        | RValue::StrConcat(arr, idx)
        | RValue::Migrate {
            actor: arr,
            node: idx,
        } => {
            out.push(*arr);
            out.push(*idx);
        }
        RValue::ArrayLit(xs) | RValue::Tuple(xs) => out.extend(xs.iter().copied()),
        RValue::Closure { captures, .. } => out.extend(captures.iter().copied()),
        RValue::Call { func, args } => {
            if let mir::FuncRef::Local(id) = func {
                out.push(*id);
            }
            out.extend(args.iter().copied());
        }
        RValue::FFICall { args, .. }
        | RValue::Perform { args, .. }
        | RValue::PerformAsync { args, .. } => out.extend(args.iter().copied()),
        RValue::Record(fields) => out.extend(fields.iter().map(|(_, id)| *id)),
        RValue::RecordUpdate { base, overrides } => {
            out.push(*base);
            out.extend(overrides.iter().map(|(_, id)| *id));
        }
        RValue::ReceiveWait { timeout, .. } => out.push(*timeout),
        RValue::Spawn {
            init, target_node, ..
        } => {
            if let Some(id) = target_node {
                out.push(*id);
            }
            for (_, rv) in init {
                rvalue_reads(rv, out);
            }
        }
        RValue::Send { actor, args, .. } | RValue::Ask { actor, args, .. } => {
            out.push(*actor);
            out.extend(args.iter().copied());
        }
    }
}

// ---------------------------------------------------------------------------
// Ownership-aware direct-call candidate analysis
// ---------------------------------------------------------------------------

/// Why a parameter cannot yet become an owned call parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOwnershipBlocker {
    PublicFunction,
    Entrypoint,
    DynamicCallable,
    NoDirectCallers,
    NonLinearParameter,
    UntransferableCallArgument,
}

/// Evidence available for one direct-call argument at the current MIR stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgTransferEvidence {
    /// Single-use compiler temporary with a counted owning definition.
    OwnedTemporary,
    /// Exactly-once parameter used only at this call. This becomes transferable
    /// only if the caller itself eventually receives that parameter as owned.
    LinearForward,
    NotTransferable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamOwnershipCandidate {
    pub param: LocalId,
    pub cap: Capability,
    pub candidate_owned: bool,
    pub requires_upstream_owned_param: bool,
    pub blockers: Vec<CallOwnershipBlocker>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnOwnershipCandidate {
    BorrowedOrImmediate,
    /// Every value-return path returns a locally-created counted owner.
    OwnedLocal,
    /// Every value-return path returns an exactly-once parameter. Activation
    /// requires that parameter to be promoted to owned first.
    OwnedFromLinearParam,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionOwnershipCandidate {
    pub function_idx: usize,
    pub name: String,
    pub direct_call_sites: usize,
    pub public: bool,
    pub dynamic_callable: bool,
    pub params: Vec<ParamOwnershipCandidate>,
    pub return_ownership: ReturnOwnershipCandidate,
}

#[derive(Debug, Clone, Copy)]
enum CallerRef {
    Function(usize),
    Behavior(usize),
}

#[derive(Debug, Clone)]
struct DirectCallSite {
    caller: CallerRef,
    args: Vec<LocalId>,
}

#[derive(Debug, Clone)]
struct CallLocalFacts {
    def_count: Vec<usize>,
    owning_def: Vec<bool>,
    use_count: Vec<usize>,
}

/// Analyze which direct-call ownership contracts are locally plausible.
///
/// This is intentionally diagnostic metadata only. It does not alter MIR,
/// bytecode, parameter Drop roots, or the VM calling convention.
pub fn analyze_call_ownership(module: &mir::Module) -> Vec<FunctionOwnershipCandidate> {
    let mut call_sites: Vec<Vec<DirectCallSite>> =
        (0..module.functions.len()).map(|_| Vec::new()).collect();
    let mut dynamic_callable = vec![false; module.functions.len()];

    for (idx, func) in module.functions.iter().enumerate() {
        collect_function_calls(
            func,
            CallerRef::Function(idx),
            &mut call_sites,
            &mut dynamic_callable,
        );
    }
    for (idx, func) in module.behaviors.iter().enumerate() {
        collect_function_calls(
            func,
            CallerRef::Behavior(idx),
            &mut call_sites,
            &mut dynamic_callable,
        );
    }

    let function_facts: Vec<CallLocalFacts> =
        module.functions.iter().map(call_local_facts).collect();
    let behavior_facts: Vec<CallLocalFacts> =
        module.behaviors.iter().map(call_local_facts).collect();

    module
        .functions
        .iter()
        .enumerate()
        .map(|(idx, func)| {
            let sites = &call_sites[idx];
            let function_blockers = function_level_blockers(func, dynamic_callable[idx], sites.len());
            let params = func
                .params
                .iter()
                .enumerate()
                .map(|(param_idx, param)| {
                    let cap = func.locals[param.0 as usize].cap;
                    let mut blockers = function_blockers.clone();
                    let linear = matches!(cap, Capability::LinearIso | Capability::Linear);
                    if !linear {
                        blockers.push(CallOwnershipBlocker::NonLinearParameter);
                    }

                    let mut requires_upstream = false;
                    if linear && blockers.iter().all(|b| {
                        !matches!(b, CallOwnershipBlocker::UntransferableCallArgument)
                    }) {
                        for site in sites {
                            let Some(arg) = site.args.get(param_idx).copied() else {
                                blockers.push(CallOwnershipBlocker::UntransferableCallArgument);
                                continue;
                            };
                            let evidence = match site.caller {
                                CallerRef::Function(caller_idx) => arg_transfer_evidence(
                                    &module.functions[caller_idx],
                                    &function_facts[caller_idx],
                                    arg,
                                ),
                                CallerRef::Behavior(caller_idx) => arg_transfer_evidence(
                                    &module.behaviors[caller_idx],
                                    &behavior_facts[caller_idx],
                                    arg,
                                ),
                            };
                            match evidence {
                                ArgTransferEvidence::OwnedTemporary => {}
                                ArgTransferEvidence::LinearForward => {
                                    requires_upstream = true;
                                }
                                ArgTransferEvidence::NotTransferable => {
                                    blockers.push(CallOwnershipBlocker::UntransferableCallArgument);
                                }
                            }
                        }
                    }
                    blockers.sort_by_key(|b| *b as u8);
                    blockers.dedup();

                    ParamOwnershipCandidate {
                        param: *param,
                        cap,
                        candidate_owned: blockers.is_empty(),
                        requires_upstream_owned_param: requires_upstream,
                        blockers,
                    }
                })
                .collect();

            FunctionOwnershipCandidate {
                function_idx: idx,
                name: func.name.clone(),
                direct_call_sites: sites.len(),
                public: func.public,
                dynamic_callable: dynamic_callable[idx],
                params,
                return_ownership: analyze_return_candidate(func, &function_facts[idx]),
            }
        })
        .collect()
}

fn function_level_blockers(
    func: &mir::Function,
    dynamic_callable: bool,
    direct_call_sites: usize,
) -> Vec<CallOwnershipBlocker> {
    let mut blockers = Vec::new();
    if func.public {
        blockers.push(CallOwnershipBlocker::PublicFunction);
    }
    if matches!(func.name.as_str(), "main" | "__main") {
        blockers.push(CallOwnershipBlocker::Entrypoint);
    }
    if dynamic_callable {
        blockers.push(CallOwnershipBlocker::DynamicCallable);
    }
    if direct_call_sites == 0 {
        blockers.push(CallOwnershipBlocker::NoDirectCallers);
    }
    blockers
}

fn collect_function_calls(
    func: &mir::Function,
    caller: CallerRef,
    call_sites: &mut [Vec<DirectCallSite>],
    dynamic_callable: &mut [bool],
) {
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign { op, .. } = stmt {
                collect_rvalue_calls(op, caller, call_sites, dynamic_callable);
            }
        }
    }
}

fn collect_rvalue_calls(
    rv: &mir::RValue,
    caller: CallerRef,
    call_sites: &mut [Vec<DirectCallSite>],
    dynamic_callable: &mut [bool],
) {
    match rv {
        mir::RValue::Call {
            func: mir::FuncRef::Index(target),
            args,
        } => {
            if let Some(sites) = call_sites.get_mut(*target) {
                sites.push(DirectCallSite {
                    caller,
                    args: args.clone(),
                });
            }
        }
        mir::RValue::Closure { func, .. } => {
            if let Some(dynamic) = dynamic_callable.get_mut(*func) {
                *dynamic = true;
            }
        }
        mir::RValue::Spawn { init, .. } => {
            for (_, nested) in init {
                collect_rvalue_calls(nested, caller, call_sites, dynamic_callable);
            }
        }
        _ => {}
    }
}

fn call_local_facts(func: &mir::Function) -> CallLocalFacts {
    let nlocals = func.locals.len();
    let transfers: FxHashSet<(LocalId, LocalId)> = func
        .ownership_transfers
        .iter()
        .map(|t| (t.src, t.dst))
        .collect();
    let mut def_count = vec![0usize; nlocals];
    let mut owning_def = vec![false; nlocals];
    let mut use_count = vec![0usize; nlocals];

    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign { dst, op } = stmt {
                let d = dst.0 as usize;
                def_count[d] += 1;
                if def_count[d] == 1 {
                    owning_def[d] = counted_owning_definition(op, &func.locals[d].ty)
                        || matches!(
                            op,
                            mir::RValue::Load(src)
                                if transfers.contains(&(*src, *dst))
                        );
                } else {
                    owning_def[d] = false;
                }
            }
            let mut reads = Vec::new();
            stmt_reads(stmt, &mut reads);
            for id in reads {
                use_count[id.0 as usize] += 1;
            }
        }
        let mut reads = Vec::new();
        terminator_reads(&block.terminator, &mut reads);
        for id in reads {
            use_count[id.0 as usize] += 1;
        }
    }

    CallLocalFacts {
        def_count,
        owning_def,
        use_count,
    }
}

fn arg_transfer_evidence(
    caller: &mir::Function,
    facts: &CallLocalFacts,
    arg: LocalId,
) -> ArgTransferEvidence {
    let i = arg.0 as usize;
    if facts.use_count.get(i).copied() != Some(1) {
        return ArgTransferEvidence::NotTransferable;
    }
    let Some(local) = caller.locals.get(i) else {
        return ArgTransferEvidence::NotTransferable;
    };

    let compiler_temp = local
        .name
        .as_deref()
        .map(|n| n.starts_with("__"))
        .unwrap_or(true);
    if compiler_temp
        && facts.def_count.get(i).copied() == Some(1)
        && facts.owning_def.get(i).copied() == Some(true)
    {
        return ArgTransferEvidence::OwnedTemporary;
    }

    if caller.params.contains(&arg)
        && matches!(local.cap, Capability::LinearIso | Capability::Linear)
    {
        return ArgTransferEvidence::LinearForward;
    }

    ArgTransferEvidence::NotTransferable
}

fn counted_owning_definition(op: &mir::RValue, ty: &Type) -> bool {
    if !definitely_counted_heap_type(ty) {
        return false;
    }
    matches!(
        op,
        mir::RValue::Tuple(_)
            | mir::RValue::Record(_)
            | mir::RValue::RecordUpdate { .. }
            | mir::RValue::ArrayLit(_)
    )
}

fn definitely_counted_heap_type(ty: &Type) -> bool {
    match ty {
        Type::Primitive(PrimitiveType::String) => true,
        Type::Tuple(_) | Type::Record(_) | Type::Array(_) | Type::Variant(_) | Type::App { .. } => true,
        Type::Nominal { underlying, .. } => definitely_counted_heap_type(underlying),
        Type::Reference { inner, .. } => definitely_counted_heap_type(inner),
        _ => false,
    }
}

fn analyze_return_candidate(
    func: &mir::Function,
    facts: &CallLocalFacts,
) -> ReturnOwnershipCandidate {
    let mut saw_value_return = false;
    let mut saw_local_owner = false;
    let mut saw_linear_param = false;

    for block in &func.blocks {
        let mir::Terminator::Return(Some(id)) = &block.terminator else {
            continue;
        };
        saw_value_return = true;
        let i = id.0 as usize;
        let local_owner = facts.def_count.get(i).copied() == Some(1)
            && facts.owning_def.get(i).copied() == Some(true);
        if local_owner {
            saw_local_owner = true;
            continue;
        }
        let linear_param = func.params.contains(id)
            && func
                .locals
                .get(i)
                .map(|l| matches!(l.cap, Capability::LinearIso | Capability::Linear))
                .unwrap_or(false);
        if linear_param {
            saw_linear_param = true;
            continue;
        }
        return ReturnOwnershipCandidate::BorrowedOrImmediate;
    }

    if !saw_value_return {
        ReturnOwnershipCandidate::BorrowedOrImmediate
    } else if saw_linear_param {
        ReturnOwnershipCandidate::OwnedFromLinearParam
    } else if saw_local_owner {
        ReturnOwnershipCandidate::OwnedLocal
    } else {
        ReturnOwnershipCandidate::BorrowedOrImmediate
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn single_function_module(func: mir::Function) -> mir::Module {
        let mut module = mir::Module::new("test");
        module.functions.push(func);
        module
    }

    fn linear_array_callee(public: bool) -> mir::Function {
        let array_ty = Type::Array(Box::new(Type::int()));
        let mut b = mir::FunctionBuilder::new("consume_array", None);
        b.set_public(public);
        let _p = b.add_param_with_cap("x", array_ty, Capability::LinearIso);
        b.terminate(mir::Terminator::Return(None));
        b.build()
    }

    #[test]
    fn call_ownership_candidate_accepts_single_use_owned_temp() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let callee = linear_array_callee(false);
        let mut caller = mir::FunctionBuilder::new("caller", None);
        let arg = caller.add_temp(array_ty);
        let result = caller.add_temp(Type::unit());
        caller.assign(arg, mir::RValue::ArrayLit(vec![]));
        caller.assign(
            result,
            mir::RValue::Call {
                func: mir::FuncRef::Index(0),
                args: vec![arg],
            },
        );
        caller.terminate(mir::Terminator::Return(None));

        let mut module = mir::Module::new("test");
        module.functions.push(callee);
        module.functions.push(caller.build());
        let report = analyze_call_ownership(&module);
        let p = &report[0].params[0];
        assert!(p.candidate_owned, "fresh single-use owner should be a candidate: {:?}", p.blockers);
        assert!(!p.requires_upstream_owned_param);
        assert_eq!(report[0].direct_call_sites, 1);
    }

    #[test]
    fn call_ownership_candidate_blocks_public_function() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let callee = linear_array_callee(true);
        let mut caller = mir::FunctionBuilder::new("caller", None);
        let arg = caller.add_temp(array_ty);
        let result = caller.add_temp(Type::unit());
        caller.assign(arg, mir::RValue::ArrayLit(vec![]));
        caller.assign(
            result,
            mir::RValue::Call {
                func: mir::FuncRef::Index(0),
                args: vec![arg],
            },
        );
        caller.terminate(mir::Terminator::Return(None));

        let mut module = mir::Module::new("test");
        module.functions.push(callee);
        module.functions.push(caller.build());
        let report = analyze_call_ownership(&module);
        let p = &report[0].params[0];
        assert!(!p.candidate_owned);
        assert!(p.blockers.contains(&CallOwnershipBlocker::PublicFunction));
    }

    #[test]
    fn call_ownership_candidate_blocks_dynamic_closure_target() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let callee = linear_array_callee(false);
        let mut caller = mir::FunctionBuilder::new("caller", None);
        let arg = caller.add_temp(array_ty);
        let clos = caller.add_temp(Type::unit());
        let result = caller.add_temp(Type::unit());
        caller.assign(arg, mir::RValue::ArrayLit(vec![]));
        caller.assign(
            clos,
            mir::RValue::Closure {
                func: 0,
                captures: vec![],
            },
        );
        caller.assign(
            result,
            mir::RValue::Call {
                func: mir::FuncRef::Index(0),
                args: vec![arg],
            },
        );
        caller.terminate(mir::Terminator::Return(None));

        let mut module = mir::Module::new("test");
        module.functions.push(callee);
        module.functions.push(caller.build());
        let report = analyze_call_ownership(&module);
        assert!(report[0].dynamic_callable);
        assert!(report[0].params[0]
            .blockers
            .contains(&CallOwnershipBlocker::DynamicCallable));
    }

    #[test]
    fn call_ownership_candidate_marks_linear_forward_dependency() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let callee = linear_array_callee(false);
        let mut caller = mir::FunctionBuilder::new("forwarder", None);
        let arg = caller.add_param_with_cap("x", array_ty, Capability::LinearIso);
        let result = caller.add_temp(Type::unit());
        caller.assign(
            result,
            mir::RValue::Call {
                func: mir::FuncRef::Index(0),
                args: vec![arg],
            },
        );
        caller.terminate(mir::Terminator::Return(None));

        let mut module = mir::Module::new("test");
        module.functions.push(callee);
        module.functions.push(caller.build());
        let report = analyze_call_ownership(&module);
        assert!(report[0].params[0].candidate_owned);
        assert!(report[0].params[0].requires_upstream_owned_param);
    }

    #[test]
    fn return_ownership_candidates_distinguish_local_and_linear_param() {
        let array_ty = Type::Array(Box::new(Type::int()));

        let mut fresh = mir::FunctionBuilder::new("fresh", Some(array_ty.clone()));
        let value = fresh.add_temp(array_ty.clone());
        fresh.assign(value, mir::RValue::ArrayLit(vec![]));
        fresh.terminate(mir::Terminator::Return(Some(value)));

        let mut forwarding = mir::FunctionBuilder::new("forward", Some(array_ty.clone()));
        let param = forwarding.add_param_with_cap("x", array_ty, Capability::LinearIso);
        forwarding.terminate(mir::Terminator::Return(Some(param)));

        let mut module = mir::Module::new("test");
        module.functions.push(fresh.build());
        module.functions.push(forwarding.build());
        let report = analyze_call_ownership(&module);
        assert_eq!(report[0].return_ownership, ReturnOwnershipCandidate::OwnedLocal);
        assert_eq!(
            report[1].return_ownership,
            ReturnOwnershipCandidate::OwnedFromLinearParam
        );
    }
    #[test]
    fn infers_transfer_for_single_use_owning_temp() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let mut b = mir::FunctionBuilder::new("f", None);
        let src = b.add_temp(array_ty.clone());
        let dst = b.add_local("result", array_ty);
        let len = b.add_temp(Type::int());
        b.assign(src, mir::RValue::ArrayLit(vec![]));
        b.assign(dst, mir::RValue::Load(src));
        b.assign(len, mir::RValue::ArrayLen(dst));
        b.terminate(mir::Terminator::Return(None));

        let mut module = single_function_module(b.build());
        assert_eq!(infer_last_use_transfers(&mut module), 1);
        let f = &module.functions[0];
        assert!(f
            .ownership_transfers
            .iter()
            .any(|t| t.src == src && t.dst == dst));

        let stmts = &f.blocks[0].stmts;
        let move_si = stmts
            .iter()
            .position(|s| {
                matches!(
                    s,
                    mir::Stmt::Assign {
                        dst: d,
                        op: mir::RValue::Load(s)
                    } if *d == dst && *s == src
                )
            })
            .expect("inferred transfer move");
        assert!(matches!(
            stmts.get(move_si + 1),
            Some(mir::Stmt::Assign {
                dst: d,
                op: mir::RValue::Const(Constant::Nil)
            }) if *d == src
        ));
    }

    #[test]
    fn does_not_infer_transfer_from_named_source() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let mut b = mir::FunctionBuilder::new("f", None);
        let src = b.add_local("source", array_ty.clone());
        let dst = b.add_temp(array_ty);
        b.assign(src, mir::RValue::ArrayLit(vec![]));
        b.assign(dst, mir::RValue::Load(src));
        b.terminate(mir::Terminator::Return(None));

        let mut module = single_function_module(b.build());
        assert_eq!(infer_last_use_transfers(&mut module), 0);
        assert!(module.functions[0].ownership_transfers.is_empty());
    }

    #[test]
    fn does_not_infer_transfer_when_source_has_multiple_reads() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let mut b = mir::FunctionBuilder::new("f", None);
        let src = b.add_temp(array_ty.clone());
        let dst = b.add_temp(array_ty);
        let len = b.add_temp(Type::int());
        b.assign(src, mir::RValue::ArrayLit(vec![]));
        b.assign(len, mir::RValue::ArrayLen(src));
        b.assign(dst, mir::RValue::Load(src));
        b.terminate(mir::Terminator::Return(None));

        let mut module = single_function_module(b.build());
        assert_eq!(infer_last_use_transfers(&mut module), 0);
    }

    #[test]
    fn infers_multi_hop_temp_copy_chain_to_fixed_point() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let mut b = mir::FunctionBuilder::new("f", None);
        let src = b.add_temp(array_ty.clone());
        let mid = b.add_temp(array_ty.clone());
        let dst = b.add_local("result", array_ty);
        b.assign(src, mir::RValue::ArrayLit(vec![]));
        b.assign(mid, mir::RValue::Load(src));
        b.assign(dst, mir::RValue::Load(mid));
        b.terminate(mir::Terminator::Return(None));

        let mut module = single_function_module(b.build());
        assert_eq!(infer_last_use_transfers(&mut module), 2);
        let transfers = &module.functions[0].ownership_transfers;
        assert!(transfers.iter().any(|t| t.src == src && t.dst == mid));
        assert!(transfers.iter().any(|t| t.src == mid && t.dst == dst));
    }

    #[test]
    fn synthetic_clears_preserve_existing_source_line_indices() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let mut b = mir::FunctionBuilder::new("f", None);
        let src = b.add_temp(array_ty.clone());
        let dst = b.add_local("result", array_ty);
        let len = b.add_temp(Type::int());

        b.set_line(10);
        b.assign(src, mir::RValue::ArrayLit(vec![]));
        b.set_line(20);
        b.assign(dst, mir::RValue::Load(src));
        b.set_line(30);
        b.assign(len, mir::RValue::ArrayLen(dst));
        b.terminate(mir::Terminator::Return(None));

        let mut module = single_function_module(b.build());
        assert_eq!(infer_last_use_transfers(&mut module), 1);
        let f = &module.functions[0];

        let line_20 = f
            .line_table
            .iter()
            .find(|(_, line)| *line == 20)
            .map(|((_, si), _)| *si)
            .expect("line 20 mapping");
        let line_30 = f
            .line_table
            .iter()
            .find(|(_, line)| *line == 30)
            .map(|((_, si), _)| *si)
            .expect("line 30 mapping");

        assert_eq!(line_20, 1, "the transfer statement keeps its index");
        assert_eq!(
            line_30, 3,
            "the following source statement shifts past the synthetic clear"
        );
    }

    #[test]
    fn inferred_transfer_can_continue_an_explicit_transfer_chain() {
        let array_ty = Type::Array(Box::new(Type::int()));
        let mut b = mir::FunctionBuilder::new("f", None);
        let src = b.add_temp(array_ty.clone());
        let mid = b.add_temp(array_ty.clone());
        let dst = b.add_temp(array_ty);
        b.assign(src, mir::RValue::ArrayLit(vec![]));
        b.transfer(mid, src);
        b.assign(dst, mir::RValue::Load(mid));
        b.terminate(mir::Terminator::Return(None));

        let mut module = single_function_module(b.build());
        assert_eq!(infer_last_use_transfers(&mut module), 1);
        assert!(module.functions[0]
            .ownership_transfers
            .iter()
            .any(|t| t.src == mid && t.dst == dst));
    }
}
