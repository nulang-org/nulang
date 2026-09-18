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
use crate::types::{PrimitiveType, Type};
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

#[cfg(test)]
mod tests {
    use super::*;

    fn single_function_module(func: mir::Function) -> mir::Module {
        let mut module = mir::Module::new("test");
        module.functions.push(func);
        module
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
