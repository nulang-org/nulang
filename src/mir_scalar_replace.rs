//! Conservative MIR scalar replacement for local immutable aggregates.
//!
//! This transform consumes the proof from `mir_escape` and removes heap
//! allocation for a deliberately narrow, obviously-safe first class:
//!
//! - immutable Tuple / Record aggregates;
//! - no aliases of the aggregate local;
//! - same-block projections occur after construction;
//! - cross-block projections are dominated by the construction block;
//! - cross-block projected values are stable parameters/captures or are
//!   defined immediately before construction in the same block;
//! - the aggregate local is anonymous or compiler-generated, preserving the
//!   optimizer's existing debugger policy for ordinary named source locals.
//!
//! Cross-block replacement uses explicit MIR dominance and deliberately opts
//! out when effect-handler tables are present because handler dispatch adds
//! implicit control-flow edges. Mutable aggregates are still deferred.

use crate::mir::{BlockId, FuncRef, Function, LocalId, RValue, Stmt, Terminator};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
struct ProjectionRewrite {
    block: BlockId,
    stmt_index: usize,
    source: LocalId,
}

#[derive(Debug, Clone)]
struct ReplacementPlan {
    block: BlockId,
    allocation_stmt: usize,
    projections: Vec<ProjectionRewrite>,
}

/// Replace safe tuple/record allocations with direct scalar loads.
///
/// Returns the number of aggregate allocations eliminated.
pub fn scalar_replace_function(function: &mut Function) -> usize {
    let summaries = crate::mir_escape::analyze_function(function);
    let dominators = crate::mir_cfg::Dominators::compute(function);
    let mut plans = Vec::new();

    for summary in summaries {
        if !summary.scalar_replaceable || summary.aliases != vec![summary.site.dst] {
            continue;
        }
        if !debug_safe_local(function, summary.site.dst) {
            continue;
        }
        if let Some(plan) = build_plan(function, summary.site, &dominators) {
            plans.push(plan);
        }
    }

    if plans.is_empty() {
        return 0;
    }

    // Rewrite projections before removing definitions so statement indices are
    // still the ones reported by the analysis.
    for plan in &plans {
        for rewrite in &plan.projections {
            let block = &mut function.blocks[rewrite.block.0 as usize];
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
            .push(plan.allocation_stmt);
    }

    for (block_id, mut indices) in removals {
        indices.sort_unstable();
        indices.dedup();
        let block = &mut function.blocks[block_id.0 as usize];
        let remove_set: BTreeSet<usize> = indices.iter().copied().collect();
        block.stmts = std::mem::take(&mut block.stmts)
            .into_iter()
            .enumerate()
            .filter_map(|(index, stmt)| (!remove_set.contains(&index)).then_some(stmt))
            .collect();
        shift_line_table_after_removals(function, block_id, &indices);
    }

    plans.len()
}

fn debug_safe_local(function: &Function, local: LocalId) -> bool {
    function
        .locals
        .get(local.0 as usize)
        .and_then(|local| local.name.as_deref())
        .map(|name| name.starts_with("__"))
        .unwrap_or(true)
}

fn build_plan(
    function: &Function,
    site: crate::mir_escape::AggregateSite,
    dominators: &crate::mir_cfg::Dominators,
) -> Option<ReplacementPlan> {
    let block = function.blocks.get(site.block.0 as usize)?;
    let definition = block.stmts.get(site.stmt_index)?;

    let fields = match definition {
        Stmt::Assign {
            dst,
            op: RValue::Tuple(values),
        } if *dst == site.dst => FieldsAdapter::Tuple(values),
        Stmt::Assign {
            dst,
            op: RValue::Record(values),
        } if *dst == site.dst => {
            // Duplicate field names make direct name->source substitution
            // ambiguous. The frontend normally rejects these, but keep this
            // transform independently conservative.
            let mut seen = BTreeSet::new();
            if values.iter().any(|(name, _)| !seen.insert(name.as_str())) {
                return None;
            }
            FieldsAdapter::Record(values)
        }
        _ => return None,
    };

    let mut projections = Vec::new();

    for other in &function.blocks {
        for (stmt_index, stmt) in other.stmts.iter().enumerate() {
            if other.id == site.block && stmt_index == site.stmt_index {
                continue;
            }

            if let Some(source) = projection_source(stmt, site.dst, &fields) {
                if projection_writes_root(stmt, site.dst) || source == site.dst {
                    return None;
                }

                let same_block = other.id == site.block;
                if same_block {
                    if stmt_index <= site.stmt_index
                        || source_reassigned_in_same_block(
                            block,
                            source,
                            site.stmt_index,
                            stmt_index,
                        )
                    {
                        return None;
                    }
                } else {
                    if !function.handler_tables.is_empty()
                        || !dominators.strict_dominates(site.block, other.id)
                        || !source_stable_across_blocks(function, block, site.stmt_index, source)
                    {
                        return None;
                    }
                }

                projections.push(ProjectionRewrite {
                    block: other.id,
                    stmt_index,
                    source,
                });
                continue;
            }

            if stmt_mentions_local(stmt, site.dst) {
                return None;
            }
        }

        if terminator_mentions_local(&other.terminator, site.dst) {
            return None;
        }
    }

    if projections.is_empty() {
        return None;
    }

    Some(ReplacementPlan {
        block: site.block,
        allocation_stmt: site.stmt_index,
        projections,
    })
}

fn projection_writes_root(stmt: &Stmt, root: LocalId) -> bool {
    matches!(stmt, Stmt::Assign { dst, .. } if *dst == root)
}

fn source_reassigned_in_same_block(
    block: &crate::mir::Block,
    source: LocalId,
    allocation_stmt: usize,
    projection_stmt: usize,
) -> bool {
    block
        .stmts
        .iter()
        .take(projection_stmt)
        .skip(allocation_stmt + 1)
        .any(|stmt| matches!(stmt, Stmt::Assign { dst, .. } if *dst == source))
}

fn source_stable_across_blocks(
    function: &Function,
    allocation_block: &crate::mir::Block,
    allocation_stmt: usize,
    source: LocalId,
) -> bool {
    let explicit_defs: Vec<(BlockId, usize)> = function
        .blocks
        .iter()
        .flat_map(|block| {
            block
                .stmts
                .iter()
                .enumerate()
                .filter_map(move |(stmt_index, stmt)| {
                    matches!(stmt, Stmt::Assign { dst, .. } if *dst == source)
                        .then_some((block.id, stmt_index))
                })
        })
        .collect();

    if function.params.contains(&source) || function.captures.contains(&source) {
        return explicit_defs.is_empty();
    }

    matches!(
        explicit_defs.as_slice(),
        [(block, stmt_index)]
            if *block == allocation_block.id && *stmt_index < allocation_stmt
    )
}

fn projection_source(
    stmt: &Stmt,
    root: LocalId,
    fields: &impl ProjectionFields,
) -> Option<LocalId> {
    let Stmt::Assign { op, .. } = stmt else {
        return None;
    };
    fields.source_for_projection(op, root)
}

trait ProjectionFields {
    fn source_for_projection(&self, op: &RValue, root: LocalId) -> Option<LocalId>;
}

impl ProjectionFields for FieldsAdapter<'_> {
    fn source_for_projection(&self, op: &RValue, root: LocalId) -> Option<LocalId> {
        match (self, op) {
            (
                FieldsAdapter::Tuple(values),
                RValue::LoadFieldPos { obj, index },
            ) if *obj == root => values.get(*index as usize).copied(),
            (
                FieldsAdapter::Record(values),
                RValue::LoadFieldNamed { obj, field },
            ) if *obj == root => values
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, source)| *source),
            _ => None,
        }
    }
}

enum FieldsAdapter<'a> {
    Tuple(&'a [LocalId]),
    Record(&'a [(String, LocalId)]),
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
    use crate::mir;
    use crate::types::Type;

    fn tuple_function(name: Option<&str>) -> (Function, LocalId) {
        let mut builder = mir::FunctionBuilder::new("tuple", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let tuple = match name {
            Some(name) => builder.add_local(name, Type::unit()),
            None => builder.add_temp(Type::unit()),
        };
        builder.assign(tuple, RValue::Tuple(vec![one]));
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldPos {
                obj: tuple,
                index: 0,
            },
        );
        builder.terminate(Terminator::Return(Some(projected)));
        (builder.build(), tuple)
    }

    #[test]
    fn replaces_anonymous_tuple_projection() {
        let (mut function, tuple) = tuple_function(None);
        assert_eq!(scalar_replace_function(&mut function), 1);
        assert!(!function.blocks[0]
            .stmts
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Assign { op: RValue::Tuple(_), .. })));
        assert!(!function.blocks[0].stmts.iter().any(|stmt| {
            matches!(
                stmt,
                Stmt::Assign {
                    op: RValue::LoadFieldPos { obj, .. },
                    ..
                } if *obj == tuple
            )
        }));
    }

    #[test]
    fn preserves_ordinary_named_local_for_debugger() {
        let (mut function, _) = tuple_function(Some("point"));
        assert_eq!(scalar_replace_function(&mut function), 0);
        assert!(function.blocks[0]
            .stmts
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Assign { op: RValue::Tuple(_), .. })));
    }

    #[test]
    fn replaces_compiler_generated_named_record() {
        let mut builder = mir::FunctionBuilder::new("record", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let record = builder.add_local("__point", Type::unit());
        builder.assign(
            record,
            RValue::Record(vec![("x".to_string(), one)]),
        );
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldNamed {
                obj: record,
                field: "x".to_string(),
            },
        );
        builder.terminate(Terminator::Return(Some(projected)));
        let mut function = builder.build();

        assert_eq!(scalar_replace_function(&mut function), 1);
        assert!(!function.blocks[0]
            .stmts
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Assign { op: RValue::Record(_), .. })));
    }

    #[test]
    fn source_reassignment_blocks_replacement() {
        let mut builder = mir::FunctionBuilder::new("reassign", Some(Type::int()));
        let value = builder.add_temp(Type::int());
        builder.assign(value, RValue::Const(Constant::Int(1)));
        let tuple = builder.add_temp(Type::unit());
        builder.assign(tuple, RValue::Tuple(vec![value]));
        builder.assign(value, RValue::Const(Constant::Int(2)));
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldPos {
                obj: tuple,
                index: 0,
            },
        );
        builder.terminate(Terminator::Return(Some(projected)));
        let mut function = builder.build();

        assert_eq!(scalar_replace_function(&mut function), 0);
    }

    #[test]
    fn line_table_shifts_when_allocation_is_removed() {
        let mut builder = mir::FunctionBuilder::new("line_table", Some(Type::int()));
        builder.set_line(10);
        let value = builder.add_temp(Type::int());
        builder.assign(value, RValue::Const(Constant::Int(1)));

        builder.set_line(11);
        let tuple = builder.add_temp(Type::unit());
        builder.assign(tuple, RValue::Tuple(vec![value]));

        builder.set_line(12);
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldPos {
                obj: tuple,
                index: 0,
            },
        );
        builder.terminate(Terminator::Return(Some(projected)));
        let mut function = builder.build();

        assert_eq!(scalar_replace_function(&mut function), 1);
        assert!(!function.line_table.iter().any(|(_, line)| *line == 11));
        assert!(function
            .line_table
            .iter()
            .any(|((block, stmt), line)| *block == BlockId(0) && *stmt == 1 && *line == 12));
    }

    #[test]
    fn cross_block_projection_is_replaced_when_dominated() {
        let mut builder = mir::FunctionBuilder::new("cross_block", Some(Type::int()));
        let value = builder.add_temp(Type::int());
        builder.assign(value, RValue::Const(Constant::Int(1)));
        let tuple = builder.add_temp(Type::unit());
        builder.assign(tuple, RValue::Tuple(vec![value]));
        let next = builder.create_block();
        builder.terminate(Terminator::Jump(next));
        builder.switch_to(next);
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldPos {
                obj: tuple,
                index: 0,
            },
        );
        builder.terminate(Terminator::Return(Some(projected)));
        let mut function = builder.build();

        assert_eq!(scalar_replace_function(&mut function), 1);
        assert!(!function.blocks[0]
            .stmts
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Assign { op: RValue::Tuple(_), .. })));
        assert!(matches!(
            function.blocks[next.0 as usize].stmts.first(),
            Some(Stmt::Assign {
                op: RValue::Load(source),
                ..
            }) if *source == value
        ));
    }

    #[test]
    fn cross_block_projection_rejects_non_dominating_definition() {
        let mut builder = mir::FunctionBuilder::new("diamond_reject", Some(Type::int()));
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
        let value = builder.add_temp(Type::int());
        builder.assign(value, RValue::Const(Constant::Int(1)));
        let tuple = builder.add_temp(Type::unit());
        builder.assign(tuple, RValue::Tuple(vec![value]));
        builder.terminate(Terminator::Jump(join));

        builder.switch_to(right);
        builder.terminate(Terminator::Jump(join));

        builder.switch_to(join);
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldPos {
                obj: tuple,
                index: 0,
            },
        );
        builder.terminate(Terminator::Return(Some(projected)));

        let mut function = builder.build();
        assert_eq!(scalar_replace_function(&mut function), 0);
    }

    #[test]
    fn cross_block_projection_rejects_unstable_source() {
        let mut builder = mir::FunctionBuilder::new("unstable_source", Some(Type::int()));
        let value = builder.add_temp(Type::int());
        builder.assign(value, RValue::Const(Constant::Int(1)));
        let tuple = builder.add_temp(Type::unit());
        builder.assign(tuple, RValue::Tuple(vec![value]));

        let next = builder.create_block();
        builder.terminate(Terminator::Jump(next));
        builder.switch_to(next);
        builder.assign(value, RValue::Const(Constant::Int(2)));
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldPos {
                obj: tuple,
                index: 0,
            },
        );
        builder.terminate(Terminator::Return(Some(projected)));

        let mut function = builder.build();
        assert_eq!(scalar_replace_function(&mut function), 0);
    }
}
