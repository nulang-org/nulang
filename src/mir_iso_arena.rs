//! Conservative MIR allocation-placement analysis for native/AOT code.
//!
//! This is intentionally stricter than the bytecode iso-arena analysis.
//! A composite literal qualifies only when every initial child is statically
//! pointer-free, the value remains activation-local, and no ambiguous
//! operation observes a live alias.

use crate::mir;
use crate::type_metadata::KnownType;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MirAllocSite {
    pub block: mir::BlockId,
    pub stmt_index: usize,
}

fn local_is_pointer_free(func: &mir::Function, id: mir::LocalId) -> bool {
    let reg = mir::FunctionBuilder::LOCAL_BASE as usize + id.0 as usize;
    matches!(
        func.type_metadata.get_type(reg),
        KnownType::Int | KnownType::Float | KnownType::Bool
    )
}

fn candidate_children_pointer_free(func: &mir::Function, rv: &mir::RValue) -> bool {
    match rv {
        mir::RValue::ArrayLit(items) | mir::RValue::Tuple(items) => {
            items.iter().all(|id| local_is_pointer_free(func, *id))
        }
        mir::RValue::Record(fields) => fields
            .iter()
            .all(|(_, id)| local_is_pointer_free(func, *id)),
        _ => false,
    }
}

fn successors(func: &mir::Function, block: mir::BlockId) -> Vec<mir::BlockId> {
    let Some(b) = func.blocks.iter().find(|b| b.id == block) else {
        return Vec::new();
    };
    match b.terminator {
        mir::Terminator::Jump(target) => vec![target],
        mir::Terminator::Branch { then_, else_, .. } => vec![then_, else_],
        _ => Vec::new(),
    }
}

fn any_alias(ids: &[mir::LocalId], aliases: &HashSet<mir::LocalId>) -> bool {
    ids.iter().any(|id| aliases.contains(id))
}

fn rvalue_escapes_or_invalidates(rv: &mir::RValue, aliases: &HashSet<mir::LocalId>) -> bool {
    match rv {
        mir::RValue::Load(_) => false,
        mir::RValue::ArrayLen(_)
        | mir::RValue::ArrayLoad { .. }
        | mir::RValue::LoadFieldNamed { .. }
        | mir::RValue::LoadFieldPos { .. } => false,
        mir::RValue::Const(_)
        | mir::RValue::Panic(_)
        | mir::RValue::SelfRef
        | mir::RValue::ReceiveCommit
        | mir::RValue::StateGet { .. } => false,
        mir::RValue::Unary(_, id)
        | mir::RValue::CapabilityCheck { val: id }
        | mir::RValue::Resume(id) => aliases.contains(id),
        mir::RValue::Binary(_, a, b)
        | mir::RValue::StringEq(a, b)
        | mir::RValue::StrConcat(a, b) => aliases.contains(a) || aliases.contains(b),
        mir::RValue::Tuple(items) | mir::RValue::ArrayLit(items) => any_alias(items, aliases),
        mir::RValue::Record(fields) => fields.iter().any(|(_, id)| aliases.contains(id)),
        mir::RValue::RecordUpdate { base, overrides } => {
            aliases.contains(base) || overrides.iter().any(|(_, id)| aliases.contains(id))
        }
        mir::RValue::Call { .. }
        | mir::RValue::Closure { .. }
        | mir::RValue::Perform { .. }
        | mir::RValue::PerformAsync { .. }
        | mir::RValue::SignalWait { .. }
        | mir::RValue::Receive
        | mir::RValue::ReceiveMatch { .. }
        | mir::RValue::ReceiveWait { .. }
        | mir::RValue::FFICall { .. }
        | mir::RValue::Migrate { .. }
        | mir::RValue::Spawn { .. }
        | mir::RValue::Send { .. }
        | mir::RValue::Ask { .. } => !aliases.is_empty(),
    }
}

fn site_qualifies(func: &mir::Function, site: MirAllocSite, dst: mir::LocalId) -> bool {
    let block_index: HashMap<mir::BlockId, usize> = func
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id, i))
        .collect();

    let mut in_sets: HashMap<(mir::BlockId, usize), HashSet<mir::LocalId>> = HashMap::new();
    let start = (site.block, site.stmt_index + 1);
    in_sets.insert(start, HashSet::from([dst]));
    let mut queue = VecDeque::from([start]);

    while let Some((bid, stmt_index)) = queue.pop_front() {
        let Some(&bi) = block_index.get(&bid) else {
            return false;
        };
        let block = &func.blocks[bi];
        let aliases = in_sets.get(&(bid, stmt_index)).cloned().unwrap_or_default();
        if aliases.is_empty() {
            continue;
        }

        if stmt_index < block.stmts.len() {
            let stmt = &block.stmts[stmt_index];
            let mut out = aliases.clone();

            match stmt {
                mir::Stmt::Assign { dst, op } => {
                    if rvalue_escapes_or_invalidates(op, &aliases) {
                        return false;
                    }
                    out.remove(dst);
                    if let mir::RValue::Load(src) = op {
                        if aliases.contains(src) {
                            out.insert(*dst);
                        }
                    }
                }
                mir::Stmt::StoreFieldNamed { obj, src, .. } => {
                    if aliases.contains(src) || aliases.contains(obj) {
                        return false;
                    }
                }
                mir::Stmt::ArrayStore { arr, idx, src } => {
                    if aliases.contains(arr) || aliases.contains(idx) || aliases.contains(src) {
                        return false;
                    }
                }
                mir::Stmt::StateSet { src, .. } => {
                    if aliases.contains(src) {
                        return false;
                    }
                }
                mir::Stmt::Emit { args, .. } => {
                    if any_alias(args, &aliases) {
                        return false;
                    }
                }
                mir::Stmt::EnterHandle { .. }
                | mir::Stmt::PopHandler
                | mir::Stmt::ParallelMarker { .. } => {}
            }

            let next = (bid, stmt_index + 1);
            let entry = in_sets.entry(next).or_default();
            let before = entry.len();
            entry.extend(out);
            if entry.len() != before {
                queue.push_back(next);
            }
            continue;
        }

        match &block.terminator {
            mir::Terminator::Return(Some(id)) | mir::Terminator::Resume(id) => {
                if aliases.contains(id) {
                    return false;
                }
            }
            mir::Terminator::Branch { .. }
            | mir::Terminator::Return(None)
            | mir::Terminator::Jump(_) => {}
            mir::Terminator::Unterminated => return false,
        }

        for succ in successors(func, bid) {
            let next = (succ, 0);
            let entry = in_sets.entry(next).or_default();
            let before = entry.len();
            entry.extend(aliases.iter().copied());
            if entry.len() != before {
                queue.push_back(next);
            }
        }
    }

    true
}

pub fn qualifying_alloc_sites(func: &mir::Function) -> HashSet<MirAllocSite> {
    let mut out = HashSet::new();
    for block in &func.blocks {
        for (stmt_index, stmt) in block.stmts.iter().enumerate() {
            let mir::Stmt::Assign { dst, op } = stmt else {
                continue;
            };
            if !matches!(
                op,
                mir::RValue::ArrayLit(_) | mir::RValue::Record(_) | mir::RValue::Tuple(_)
            ) {
                continue;
            }
            if !candidate_children_pointer_free(func, op) {
                continue;
            }
            let site = MirAllocSite {
                block: block.id,
                stmt_index,
            };
            if site_qualifies(func, site, *dst) {
                out.insert(site);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Constant;
    use crate::types::{PrimitiveType, Type};

    fn array_of(inner: Type) -> Type {
        Type::Array(Box::new(inner))
    }

    #[test]
    fn local_scalar_array_qualifies() {
        let mut b = mir::FunctionBuilder::new("f", Some(Type::Primitive(PrimitiveType::Int)));
        let one = b.add_temp(Type::Primitive(PrimitiveType::Int));
        b.assign(one, mir::RValue::Const(Constant::Int(1)));
        let arr = b.add_temp(array_of(Type::Primitive(PrimitiveType::Int)));
        b.assign(arr, mir::RValue::ArrayLit(vec![one]));
        let idx = b.add_temp(Type::Primitive(PrimitiveType::Int));
        b.assign(idx, mir::RValue::Const(Constant::Int(0)));
        let first = b.add_temp(Type::Primitive(PrimitiveType::Int));
        b.assign(first, mir::RValue::ArrayLoad { arr, idx });
        b.terminate(mir::Terminator::Return(Some(first)));
        let f = b.build();

        assert!(qualifying_alloc_sites(&f).contains(&MirAllocSite {
            block: mir::BlockId(0),
            stmt_index: 1,
        }));
    }

    #[test]
    fn returned_array_is_rejected() {
        let mut b = mir::FunctionBuilder::new("f", None);
        let one = b.add_temp(Type::Primitive(PrimitiveType::Int));
        b.assign(one, mir::RValue::Const(Constant::Int(1)));
        let arr = b.add_temp(array_of(Type::Primitive(PrimitiveType::Int)));
        b.assign(arr, mir::RValue::ArrayLit(vec![one]));
        b.terminate(mir::Terminator::Return(Some(arr)));
        let f = b.build();

        assert!(qualifying_alloc_sites(&f).is_empty());
    }

    #[test]
    fn pointer_child_is_rejected() {
        let mut b = mir::FunctionBuilder::new("f", None);
        let child = b.add_temp(Type::Primitive(PrimitiveType::String));
        b.assign(child, mir::RValue::Const(Constant::String("x".into())));
        let arr = b.add_temp(array_of(Type::Primitive(PrimitiveType::String)));
        b.assign(arr, mir::RValue::ArrayLit(vec![child]));
        b.terminate(mir::Terminator::Return(None));
        let f = b.build();

        assert!(qualifying_alloc_sites(&f).is_empty());
    }

    #[test]
    fn alias_stored_to_state_is_rejected() {
        let mut b = mir::FunctionBuilder::new("f", None);
        let one = b.add_temp(Type::Primitive(PrimitiveType::Int));
        b.assign(one, mir::RValue::Const(Constant::Int(1)));
        let arr_ty = array_of(Type::Primitive(PrimitiveType::Int));
        let arr = b.add_temp(arr_ty.clone());
        b.assign(arr, mir::RValue::ArrayLit(vec![one]));
        let alias = b.add_temp(arr_ty);
        b.assign(alias, mir::RValue::Load(arr));
        b.emit(mir::Stmt::StateSet {
            field: "x".into(),
            src: alias,
        });
        b.terminate(mir::Terminator::Return(None));
        let f = b.build();

        assert!(qualifying_alloc_sites(&f).is_empty());
    }
}
