//! Conservative scalar replacement for mutable MIR records.
//!
//! This pass handles the first mutable SROA case without introducing phi
//! nodes: one record allocation and all of its field mutations/projections
//! must stay in a single basic block. Each field is modeled as a sequence of
//! scalar versions. A `StoreFieldNamed` advances the current version for that
//! field; a later `LoadFieldNamed` is rewritten to load the current scalar.
//!
//! The pass fails closed for aliases, escapes, cross-block record uses,
//! unknown fields, aggregate reassignment, and debugger-visible source locals.

use crate::mir::{BlockId, FuncRef, Function, LocalId, RValue, Stmt, Terminator};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy)]
struct FieldVersion {
    source: LocalId,
    defined_at: usize,
}

#[derive(Debug, Clone, Copy)]
struct ProjectionRewrite {
    stmt_index: usize,
    source: LocalId,
}

#[derive(Debug, Clone)]
struct MutableRecordPlan {
    block: BlockId,
    rewrites: Vec<ProjectionRewrite>,
    removals: Vec<usize>,
}

/// Scalar-replace non-escaping mutable records whose complete lifetime stays
/// in one basic block.
///
/// Returns the number of record allocations eliminated.
pub fn scalar_replace_mutable_records(function: &mut Function) -> usize {
    let summaries = crate::mir_escape::analyze_function(function);
    let mut plans = Vec::new();

    for summary in summaries {
        if summary.site.kind != crate::mir_escape::AggregateKind::Record
            || summary.escapes
            || summary.aliases != vec![summary.site.dst]
            || !debug_safe_local(function, summary.site.dst)
        {
            continue;
        }

        if let Some(plan) = build_plan(function, summary.site) {
            plans.push(plan);
        }
    }

    if plans.is_empty() {
        return 0;
    }

    for plan in &plans {
        let block = &mut function.blocks[plan.block.0 as usize];
        for rewrite in &plan.rewrites {
            if let Stmt::Assign { op, .. } = &mut block.stmts[rewrite.stmt_index] {
                *op = RValue::Load(rewrite.source);
            }
        }
    }

    let mut removals: BTreeMap<BlockId, Vec<usize>> = BTreeMap::new();
    for plan in &plans {
        removals
            .entry(plan.block)
            .or_default()
            .extend(plan.removals.iter().copied());
    }

    for (block_id, mut indices) in removals {
        indices.sort_unstable();
        indices.dedup();
        let remove_set: BTreeSet<usize> = indices.iter().copied().collect();
        let block = &mut function.blocks[block_id.0 as usize];
        block.stmts = std::mem::take(&mut block.stmts)
            .into_iter()
            .enumerate()
            .filter_map(|(index, stmt)| (!remove_set.contains(&index)).then_some(stmt))
            .collect();
        shift_line_table_after_removals(function, block_id, &indices);
    }

    plans.len()
}

fn build_plan(
    function: &Function,
    site: crate::mir_escape::AggregateSite,
) -> Option<MutableRecordPlan> {
    let block = function.blocks.get(site.block.0 as usize)?;
    let definition = block.stmts.get(site.stmt_index)?;

    let Stmt::Assign {
        dst,
        op: RValue::Record(initial_fields),
    } = definition
    else {
        return None;
    };
    if *dst != site.dst {
        return None;
    }

    let mut fields = BTreeMap::<String, FieldVersion>::new();
    for (name, source) in initial_fields {
        if *source == site.dst
            || fields
                .insert(
                    name.clone(),
                    FieldVersion {
                        source: *source,
                        defined_at: site.stmt_index,
                    },
                )
                .is_some()
        {
            return None;
        }
    }

    // Mutable v1 deliberately does not synthesize phi nodes. Any root use
    // outside the construction block leaves the record materialized.
    for other in &function.blocks {
        if other.id == site.block {
            continue;
        }
        if other
            .stmts
            .iter()
            .any(|stmt| stmt_mentions_local(stmt, site.dst))
            || terminator_mentions_local(&other.terminator, site.dst)
        {
            return None;
        }
    }

    let mut rewrites = Vec::new();
    let mut removals = vec![site.stmt_index];
    let mut saw_mutation = false;

    for (stmt_index, stmt) in block
        .stmts
        .iter()
        .enumerate()
        .skip(site.stmt_index + 1)
    {
        match stmt {
            Stmt::Assign {
                dst,
                op: RValue::LoadFieldNamed { obj, field },
            } if *obj == site.dst => {
                if *dst == site.dst {
                    return None;
                }
                let version = *fields.get(field)?;
                if version.source == site.dst
                    || source_reassigned_between(
                        block,
                        version.source,
                        version.defined_at,
                        stmt_index,
                    )
                {
                    return None;
                }
                rewrites.push(ProjectionRewrite {
                    stmt_index,
                    source: version.source,
                });
            }
            Stmt::StoreFieldNamed {
                obj,
                field,
                src,
            } if *obj == site.dst => {
                if *src == site.dst || !fields.contains_key(field) {
                    return None;
                }
                fields.insert(
                    field.clone(),
                    FieldVersion {
                        source: *src,
                        defined_at: stmt_index,
                    },
                );
                removals.push(stmt_index);
                saw_mutation = true;
            }
            _ if stmt_mentions_local(stmt, site.dst) => return None,
            _ => {}
        }
    }

    if terminator_mentions_local(&block.terminator, site.dst) || !saw_mutation {
        return None;
    }

    Some(MutableRecordPlan {
        block: site.block,
        rewrites,
        removals,
    })
}

fn debug_safe_local(function: &Function, local: LocalId) -> bool {
    function
        .locals
        .get(local.0 as usize)
        .and_then(|local| local.name.as_deref())
        .map(|name| name.starts_with("__"))
        .unwrap_or(true)
}

fn source_reassigned_between(
    block: &crate::mir::Block,
    source: LocalId,
    version_stmt: usize,
    projection_stmt: usize,
) -> bool {
    block
        .stmts
        .iter()
        .take(projection_stmt)
        .skip(version_stmt + 1)
        .any(|stmt| matches!(stmt, Stmt::Assign { dst, .. } if *dst == source))
}

fn stmt_mentions_local(stmt: &Stmt, local: LocalId) -> bool {
    match stmt {
        Stmt::Assign { dst, op } => *dst == local || rvalue_mentions_local(op, local),
        Stmt::StoreFieldNamed { obj, src, .. } => *obj == local || *src == local,
        Stmt::ArrayStore { arr, idx, src } => {
            *arr == local || *idx == local || *src == local
        }
        Stmt::Emit { args, .. } => args.contains(&local),
        Stmt::StateSet { src, .. } => *src == local,
        Stmt::EnterHandle { .. } | Stmt::PopHandler => false,
    }
}

fn terminator_mentions_local(term: &Terminator, local: LocalId) -> bool {
    match term {
        Terminator::Return(Some(value)) | Terminator::Resume(value) => *value == local,
        Terminator::Branch { cond, .. } => *cond == local,
        Terminator::Return(None) | Terminator::Jump(_) | Terminator::Unterminated => false,
    }
}

fn rvalue_mentions_local(op: &RValue, local: LocalId) -> bool {
    match op {
        RValue::Const(_)
        | RValue::Panic(_)
        | RValue::SignalWait { .. }
        | RValue::Receive
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveCommit
        | RValue::SelfRef
        | RValue::StateGet { .. } => false,
        RValue::Load(value)
        | RValue::ArrayLen(value)
        | RValue::Unary(_, value)
        | RValue::Resume(value)
        | RValue::CapabilityCheck { val: value } => *value == local,
        RValue::LoadFieldNamed { obj, .. } | RValue::LoadFieldPos { obj, .. } => *obj == local,
        RValue::ArrayLoad { arr, idx }
        | RValue::Binary(_, arr, idx)
        | RValue::StringEq(arr, idx)
        | RValue::StrConcat(arr, idx)
        | RValue::Migrate {
            actor: arr,
            node: idx,
        } => *arr == local || *idx == local,
        RValue::ArrayLit(values) | RValue::Tuple(values) => values.contains(&local),
        RValue::Record(values) => values.iter().any(|(_, value)| *value == local),
        RValue::RecordUpdate { base, overrides } => {
            *base == local || overrides.iter().any(|(_, value)| *value == local)
        }
        RValue::Call { func, args } => {
            matches!(func, FuncRef::Local(value) if *value == local) || args.contains(&local)
        }
        RValue::Closure { captures, .. } => captures.contains(&local),
        RValue::Perform { args, .. }
        | RValue::PerformAsync { args, .. }
        | RValue::FFICall { args, .. } => args.contains(&local),
        RValue::ReceiveWait { timeout, .. } => *timeout == local,
        RValue::Spawn {
            init,
            target_node,
            ..
        } => {
            target_node.is_some_and(|value| value == local)
                || init
                    .iter()
                    .any(|(_, value)| rvalue_mentions_local(value, local))
        }
        RValue::Send { actor, args, .. } | RValue::Ask { actor, args, .. } => {
            *actor == local || args.contains(&local)
        }
    }
}

fn shift_line_table_after_removals(function: &mut Function, block: BlockId, removed: &[usize]) {
    function.line_table = function
        .line_table
        .iter()
        .filter_map(|&((entry_block, stmt_index), line)| {
            if entry_block != block {
                return Some(((entry_block, stmt_index), line));
            }
            if removed.binary_search(&stmt_index).is_ok() {
                return None;
            }
            let shift = removed.partition_point(|&index| index < stmt_index);
            Some(((entry_block, stmt_index - shift), line))
        })
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Constant;
    use crate::mir::{self, FunctionBuilder};
    use crate::types::Type;

    #[test]
    fn mutable_record_field_versions_replace_allocation_and_stores() {
        let mut builder = FunctionBuilder::new("mutable_record", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let two = builder.add_temp(Type::int());
        builder.assign(two, RValue::Const(Constant::Int(2)));

        let record = builder.add_local("__record", Type::unit());
        builder.assign(
            record,
            RValue::Record(vec![
                ("x".to_string(), one),
                ("y".to_string(), two),
            ]),
        );

        let before = builder.add_temp(Type::int());
        builder.assign(
            before,
            RValue::LoadFieldNamed {
                obj: record,
                field: "x".to_string(),
            },
        );

        let ten = builder.add_temp(Type::int());
        builder.assign(ten, RValue::Const(Constant::Int(10)));
        builder.emit(Stmt::StoreFieldNamed {
            obj: record,
            field: "x".to_string(),
            src: ten,
        });

        let after = builder.add_temp(Type::int());
        builder.assign(
            after,
            RValue::LoadFieldNamed {
                obj: record,
                field: "x".to_string(),
            },
        );
        builder.terminate(Terminator::Return(Some(after)));
        let mut function = builder.build();

        assert_eq!(scalar_replace_mutable_records(&mut function), 1);
        assert!(!function.blocks[0].stmts.iter().any(|stmt| {
            matches!(stmt, Stmt::Assign { op: RValue::Record(_), .. })
                || matches!(stmt, Stmt::StoreFieldNamed { obj, .. } if *obj == record)
        }));
        assert!(function.blocks[0].stmts.iter().any(|stmt| {
            matches!(
                stmt,
                Stmt::Assign {
                    dst,
                    op: RValue::Load(source),
                } if *dst == before && *source == one
            )
        }));
        assert!(function.blocks[0].stmts.iter().any(|stmt| {
            matches!(
                stmt,
                Stmt::Assign {
                    dst,
                    op: RValue::Load(source),
                } if *dst == after && *source == ten
            )
        }));
    }

    #[test]
    fn mutable_record_rejects_cross_block_use_without_phi_versions() {
        let mut builder = FunctionBuilder::new("mutable_cross_block", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let record = builder.add_local("__record", Type::unit());
        builder.assign(
            record,
            RValue::Record(vec![("x".to_string(), one)]),
        );

        let ten = builder.add_temp(Type::int());
        builder.assign(ten, RValue::Const(Constant::Int(10)));
        builder.emit(Stmt::StoreFieldNamed {
            obj: record,
            field: "x".to_string(),
            src: ten,
        });

        let next = builder.create_block();
        builder.terminate(Terminator::Jump(next));
        builder.switch_to(next);
        let result = builder.add_temp(Type::int());
        builder.assign(
            result,
            RValue::LoadFieldNamed {
                obj: record,
                field: "x".to_string(),
            },
        );
        builder.terminate(Terminator::Return(Some(result)));
        let mut function = builder.build();

        assert_eq!(scalar_replace_mutable_records(&mut function), 0);
    }

    #[test]
    fn mutable_record_rejects_source_reassignment_after_store() {
        let mut builder = FunctionBuilder::new("mutable_reassign", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let record = builder.add_local("__record", Type::unit());
        builder.assign(
            record,
            RValue::Record(vec![("x".to_string(), one)]),
        );

        let value = builder.add_temp(Type::int());
        builder.assign(value, RValue::Const(Constant::Int(10)));
        builder.emit(Stmt::StoreFieldNamed {
            obj: record,
            field: "x".to_string(),
            src: value,
        });
        builder.assign(value, RValue::Const(Constant::Int(11)));

        let result = builder.add_temp(Type::int());
        builder.assign(
            result,
            RValue::LoadFieldNamed {
                obj: record,
                field: "x".to_string(),
            },
        );
        builder.terminate(Terminator::Return(Some(result)));
        let mut function = builder.build();

        assert_eq!(scalar_replace_mutable_records(&mut function), 0);
    }

    #[test]
    fn named_user_record_remains_materialized_for_debugger() {
        let mut builder = mir::FunctionBuilder::new("debug_record", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let record = builder.add_local("record", Type::unit());
        builder.assign(
            record,
            RValue::Record(vec![("x".to_string(), one)]),
        );
        builder.emit(Stmt::StoreFieldNamed {
            obj: record,
            field: "x".to_string(),
            src: one,
        });
        let result = builder.add_temp(Type::int());
        builder.assign(
            result,
            RValue::LoadFieldNamed {
                obj: record,
                field: "x".to_string(),
            },
        );
        builder.terminate(Terminator::Return(Some(result)));
        let mut function = builder.build();

        assert_eq!(scalar_replace_mutable_records(&mut function), 0);
    }
}
