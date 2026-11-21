//! Conservative MIR aggregate escape analysis.
//!
//! This pass is intentionally analysis-only. It identifies aggregate
//! allocations that stay local to a function and, more narrowly, immutable
//! tuple/record allocations whose uses are projection-only. The latter are
//! safe candidates for a future scalar-replacement pass.
//!
//! The analysis is conservative by design:
//! - aliases are propagated through `RValue::Load`;
//! - aliases are never killed after a later overwrite, which can only turn a
//!   potential optimization into a rejection;
//! - consumers that substitute values must still prove definition/order or
//!   dominance constraints; this module reports escape, not SSA validity;
//! - crossing a call/effect/actor/FFI/state/event boundary is treated as an
//!   escape;
//! - embedding the aggregate into another aggregate is treated as an escape.
//!
//! This lives at MIR rather than bytecode so the same proof can eventually be
//! consumed by bytecode, JIT, AOT, and WASM lowering.

use crate::mir::{BlockId, FuncRef, Function, LocalId, RValue, Stmt, Terminator};
use std::collections::BTreeSet;

/// Aggregate allocation represented by a MIR rvalue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    Array,
    Tuple,
    Record,
    RecordUpdate,
}

/// Source location of one aggregate allocation in MIR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateSite {
    pub block: BlockId,
    pub stmt_index: usize,
    pub dst: LocalId,
    pub kind: AggregateKind,
}

/// First boundary that proves an aggregate escapes its activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeReason {
    Return,
    Resume,
    Call,
    ClosureCapture,
    AggregateStorage,
    EffectBoundary,
    FfiBoundary,
    ActorBoundary,
    StateStorage,
    EventEmission,
}

/// Escape/scalar-replacement summary for one aggregate site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateEscapeSummary {
    pub site: AggregateSite,
    /// Conservative alias closure rooted at `site.dst`.
    pub aliases: Vec<LocalId>,
    /// True when any alias crosses the function/activation boundary.
    pub escapes: bool,
    /// True only for immutable tuple/record values whose aggregate uses are
    /// aliases plus field projections. This is deliberately narrower than
    /// `!escapes` and is the intended input to scalar replacement.
    pub scalar_replaceable: bool,
    pub escape_reason: Option<EscapeReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UseClass {
    None,
    Projection,
    Materialized,
    Escape(EscapeReason),
}

/// Analyze every aggregate allocation in `function`.
pub fn analyze_function(function: &Function) -> Vec<AggregateEscapeSummary> {
    aggregate_sites(function)
        .into_iter()
        .map(|site| analyze_site(function, site))
        .collect()
}

/// Return the aggregate sites that the conservative analysis considers safe
/// inputs to a future scalar-replacement transform.
pub fn scalar_replacement_candidates(function: &Function) -> Vec<AggregateSite> {
    analyze_function(function)
        .into_iter()
        .filter(|summary| summary.scalar_replaceable)
        .map(|summary| summary.site)
        .collect()
}

fn aggregate_sites(function: &Function) -> Vec<AggregateSite> {
    let mut sites = Vec::new();
    for block in &function.blocks {
        for (stmt_index, stmt) in block.stmts.iter().enumerate() {
            let Stmt::Assign { dst, op } = stmt else {
                continue;
            };
            let kind = match op {
                RValue::ArrayLit(_) => AggregateKind::Array,
                RValue::Tuple(_) => AggregateKind::Tuple,
                RValue::Record(_) => AggregateKind::Record,
                RValue::RecordUpdate { .. } => AggregateKind::RecordUpdate,
                _ => continue,
            };
            sites.push(AggregateSite {
                block: block.id,
                stmt_index,
                dst: *dst,
                kind,
            });
        }
    }
    sites
}

fn alias_closure(function: &Function, root: LocalId) -> BTreeSet<LocalId> {
    let mut aliases = BTreeSet::from([root]);

    loop {
        let mut changed = false;
        for block in &function.blocks {
            for stmt in &block.stmts {
                if let Stmt::Assign {
                    dst,
                    op: RValue::Load(src),
                } = stmt
                {
                    if aliases.contains(src) {
                        changed |= aliases.insert(*dst);
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    aliases
}

fn analyze_site(function: &Function, site: AggregateSite) -> AggregateEscapeSummary {
    let aliases = alias_closure(function, site.dst);
    let mut scalar_replaceable = matches!(site.kind, AggregateKind::Tuple | AggregateKind::Record);
    let mut escape_reason = None;

    'scan: for block in &function.blocks {
        for (stmt_index, stmt) in block.stmts.iter().enumerate() {
            // The defining assignment creates the candidate; its operands do
            // not contain the newly-created value.
            if block.id == site.block && stmt_index == site.stmt_index {
                continue;
            }

            match classify_stmt_use(stmt, &aliases) {
                UseClass::None | UseClass::Projection => {}
                UseClass::Materialized => scalar_replaceable = false,
                UseClass::Escape(reason) => {
                    scalar_replaceable = false;
                    escape_reason = Some(reason);
                    break 'scan;
                }
            }
        }

        if escape_reason.is_some() {
            break;
        }

        match classify_terminator_use(&block.terminator, &aliases) {
            UseClass::None | UseClass::Projection => {}
            UseClass::Materialized => scalar_replaceable = false,
            UseClass::Escape(reason) => {
                scalar_replaceable = false;
                escape_reason = Some(reason);
                break;
            }
        }
    }

    AggregateEscapeSummary {
        site,
        aliases: aliases.into_iter().collect(),
        escapes: escape_reason.is_some(),
        scalar_replaceable,
        escape_reason,
    }
}

fn classify_stmt_use(stmt: &Stmt, aliases: &BTreeSet<LocalId>) -> UseClass {
    match stmt {
        Stmt::Assign { op, .. } => classify_rvalue_use(op, aliases),
        Stmt::StoreFieldNamed { obj, src, .. } => {
            if aliases.contains(src) {
                UseClass::Escape(EscapeReason::AggregateStorage)
            } else if aliases.contains(obj) {
                // Mutating a local record does not make it escape, but the
                // first scalar-replacement pass should stay immutable-only.
                UseClass::Materialized
            } else {
                UseClass::None
            }
        }
        Stmt::ArrayStore { arr, idx, src } => {
            if aliases.contains(src) {
                UseClass::Escape(EscapeReason::AggregateStorage)
            } else if aliases.contains(arr) || aliases.contains(idx) {
                UseClass::Materialized
            } else {
                UseClass::None
            }
        }
        Stmt::Emit { args, .. } => {
            if any_alias(args, aliases) {
                UseClass::Escape(EscapeReason::EventEmission)
            } else {
                UseClass::None
            }
        }
        Stmt::StateSet { src, .. } => {
            if aliases.contains(src) {
                UseClass::Escape(EscapeReason::StateStorage)
            } else {
                UseClass::None
            }
        }
        Stmt::EnterHandle { .. } | Stmt::PopHandler => UseClass::None,
    }
}

fn classify_terminator_use(terminator: &Terminator, aliases: &BTreeSet<LocalId>) -> UseClass {
    match terminator {
        Terminator::Return(Some(value)) if aliases.contains(value) => {
            UseClass::Escape(EscapeReason::Return)
        }
        Terminator::Resume(value) if aliases.contains(value) => {
            UseClass::Escape(EscapeReason::Resume)
        }
        Terminator::Branch { cond, .. } if aliases.contains(cond) => UseClass::Materialized,
        Terminator::Return(_)
        | Terminator::Jump(_)
        | Terminator::Branch { .. }
        | Terminator::Resume(_)
        | Terminator::Unterminated => UseClass::None,
    }
}

fn classify_rvalue_use(op: &RValue, aliases: &BTreeSet<LocalId>) -> UseClass {
    match op {
        RValue::Load(src) => {
            if aliases.contains(src) {
                UseClass::Projection
            } else {
                UseClass::None
            }
        }
        RValue::LoadFieldNamed { obj, .. } | RValue::LoadFieldPos { obj, .. } => {
            if aliases.contains(obj) {
                UseClass::Projection
            } else {
                UseClass::None
            }
        }
        RValue::ArrayLoad { arr, idx } => {
            if aliases.contains(arr) || aliases.contains(idx) {
                UseClass::Materialized
            } else {
                UseClass::None
            }
        }
        RValue::ArrayLen(array) => {
            if aliases.contains(array) {
                UseClass::Materialized
            } else {
                UseClass::None
            }
        }
        RValue::Call { func, args } => {
            let func_alias = matches!(func, FuncRef::Local(local) if aliases.contains(local));
            if func_alias || any_alias(args, aliases) {
                UseClass::Escape(EscapeReason::Call)
            } else {
                UseClass::None
            }
        }
        RValue::Closure { captures, .. } => {
            if any_alias(captures, aliases) {
                UseClass::Escape(EscapeReason::ClosureCapture)
            } else {
                UseClass::None
            }
        }
        RValue::Tuple(items) | RValue::ArrayLit(items) => {
            if any_alias(items, aliases) {
                UseClass::Escape(EscapeReason::AggregateStorage)
            } else {
                UseClass::None
            }
        }
        RValue::Record(fields) => {
            if fields.iter().any(|(_, value)| aliases.contains(value)) {
                UseClass::Escape(EscapeReason::AggregateStorage)
            } else {
                UseClass::None
            }
        }
        RValue::RecordUpdate { base, overrides } => {
            if overrides.iter().any(|(_, value)| aliases.contains(value)) {
                UseClass::Escape(EscapeReason::AggregateStorage)
            } else if aliases.contains(base) {
                UseClass::Materialized
            } else {
                UseClass::None
            }
        }
        RValue::Perform { args, .. } | RValue::PerformAsync { args, .. } => {
            if any_alias(args, aliases) {
                UseClass::Escape(EscapeReason::EffectBoundary)
            } else {
                UseClass::None
            }
        }
        RValue::FFICall { args, .. } => {
            if any_alias(args, aliases) {
                UseClass::Escape(EscapeReason::FfiBoundary)
            } else {
                UseClass::None
            }
        }
        RValue::Migrate { actor, node } => {
            if aliases.contains(actor) || aliases.contains(node) {
                UseClass::Escape(EscapeReason::ActorBoundary)
            } else {
                UseClass::None
            }
        }
        RValue::Spawn {
            init, target_node, ..
        } => {
            let init_alias = init
                .iter()
                .any(|(_, value)| rvalue_mentions_alias(value, aliases));
            let target_alias = target_node
                .as_ref()
                .is_some_and(|value| aliases.contains(value));
            if init_alias || target_alias {
                UseClass::Escape(EscapeReason::ActorBoundary)
            } else {
                UseClass::None
            }
        }
        RValue::Send { actor, args, .. } | RValue::Ask { actor, args, .. } => {
            if aliases.contains(actor) || any_alias(args, aliases) {
                UseClass::Escape(EscapeReason::ActorBoundary)
            } else {
                UseClass::None
            }
        }
        RValue::Resume(value) => {
            if aliases.contains(value) {
                UseClass::Escape(EscapeReason::Resume)
            } else {
                UseClass::None
            }
        }
        // These operations can inspect or transform the value but do not
        // transfer ownership. They keep the object local, while preventing
        // the initial projection-only scalar-replacement transform.
        RValue::Unary(_, value) | RValue::CapabilityCheck { val: value } => {
            if aliases.contains(value) {
                UseClass::Materialized
            } else {
                UseClass::None
            }
        }
        RValue::Binary(_, left, right)
        | RValue::StringEq(left, right)
        | RValue::StrConcat(left, right) => {
            if aliases.contains(left) || aliases.contains(right) {
                UseClass::Materialized
            } else {
                UseClass::None
            }
        }
        RValue::ReceiveWait { timeout, .. } => {
            if aliases.contains(timeout) {
                UseClass::Materialized
            } else {
                UseClass::None
            }
        }
        RValue::Const(_)
        | RValue::Panic(_)
        | RValue::Receive
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveCommit
        | RValue::SelfRef
        | RValue::StateGet { .. }
        | RValue::SignalWait { .. } => UseClass::None,
    }
}

fn any_alias(values: &[LocalId], aliases: &BTreeSet<LocalId>) -> bool {
    values.iter().any(|value| aliases.contains(value))
}

fn rvalue_mentions_alias(op: &RValue, aliases: &BTreeSet<LocalId>) -> bool {
    match op {
        RValue::Load(value)
        | RValue::ArrayLen(value)
        | RValue::Unary(_, value)
        | RValue::Resume(value) => aliases.contains(value),
        RValue::LoadFieldNamed { obj, .. } | RValue::LoadFieldPos { obj, .. } => {
            aliases.contains(obj)
        }
        RValue::ArrayLoad { arr, idx } => aliases.contains(arr) || aliases.contains(idx),
        RValue::ArrayLit(values) | RValue::Tuple(values) => any_alias(values, aliases),
        RValue::Binary(_, left, right)
        | RValue::StringEq(left, right)
        | RValue::StrConcat(left, right) => aliases.contains(left) || aliases.contains(right),
        RValue::Call { func, args } => {
            matches!(func, FuncRef::Local(local) if aliases.contains(local))
                || any_alias(args, aliases)
        }
        RValue::Closure { captures, .. } => any_alias(captures, aliases),
        RValue::Record(fields) => fields.iter().any(|(_, value)| aliases.contains(value)),
        RValue::RecordUpdate { base, overrides } => {
            aliases.contains(base) || overrides.iter().any(|(_, value)| aliases.contains(value))
        }
        RValue::Perform { args, .. } | RValue::PerformAsync { args, .. } => {
            any_alias(args, aliases)
        }
        RValue::ReceiveWait { timeout, .. } => aliases.contains(timeout),
        RValue::FFICall { args, .. } => any_alias(args, aliases),
        RValue::Migrate { actor, node } => aliases.contains(actor) || aliases.contains(node),
        RValue::CapabilityCheck { val } => aliases.contains(val),
        RValue::Spawn {
            init, target_node, ..
        } => {
            target_node
                .as_ref()
                .is_some_and(|value| aliases.contains(value))
                || init
                    .iter()
                    .any(|(_, value)| rvalue_mentions_alias(value, aliases))
        }
        RValue::Send { actor, args, .. } | RValue::Ask { actor, args, .. } => {
            aliases.contains(actor) || any_alias(args, aliases)
        }
        RValue::Const(_)
        | RValue::Panic(_)
        | RValue::SignalWait { .. }
        | RValue::Receive
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveCommit
        | RValue::SelfRef
        | RValue::StateGet { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Constant;
    use crate::mir::FunctionBuilder;
    use crate::types::Type;

    fn finish(mut builder: FunctionBuilder, value: Option<LocalId>) -> Function {
        builder.terminate(Terminator::Return(value));
        builder.build()
    }

    #[test]
    fn tuple_projection_is_scalar_replacement_candidate() {
        let mut builder = FunctionBuilder::new("tuple_projection", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let tuple = builder.add_temp(Type::unit());
        builder.assign(tuple, RValue::Tuple(vec![one]));
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldPos {
                obj: tuple,
                index: 0,
            },
        );

        let function = finish(builder, Some(projected));
        let summaries = analyze_function(&function);
        let summary = summaries
            .iter()
            .find(|summary| summary.site.dst == tuple)
            .unwrap();

        assert!(!summary.escapes);
        assert!(summary.scalar_replaceable);
        assert_eq!(summary.escape_reason, None);
    }

    #[test]
    fn alias_return_is_an_escape() {
        let mut builder = FunctionBuilder::new("alias_return", Some(Type::unit()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let tuple = builder.add_temp(Type::unit());
        builder.assign(tuple, RValue::Tuple(vec![one]));
        let alias = builder.add_temp(Type::unit());
        builder.assign(alias, RValue::Load(tuple));

        let function = finish(builder, Some(alias));
        let summaries = analyze_function(&function);
        let summary = summaries
            .iter()
            .find(|summary| summary.site.dst == tuple)
            .unwrap();

        assert!(summary.escapes);
        assert!(!summary.scalar_replaceable);
        assert_eq!(summary.escape_reason, Some(EscapeReason::Return));
        assert!(summary.aliases.contains(&alias));
    }

    #[test]
    fn embedding_aggregate_in_another_aggregate_escapes() {
        let mut builder = FunctionBuilder::new("nested", Some(Type::unit()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));

        let record = builder.add_temp(Type::unit());
        builder.assign(record, RValue::Record(vec![("x".to_string(), one)]));

        let outer = builder.add_temp(Type::unit());
        builder.assign(outer, RValue::Tuple(vec![record]));

        let function = finish(builder, Some(outer));
        let summaries = analyze_function(&function);
        let summary = summaries
            .iter()
            .find(|summary| summary.site.dst == record)
            .unwrap();

        assert!(summary.escapes);
        assert_eq!(summary.escape_reason, Some(EscapeReason::AggregateStorage));
    }

    #[test]
    fn record_projection_is_scalar_replacement_candidate() {
        let mut builder = FunctionBuilder::new("record_projection", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let record = builder.add_temp(Type::unit());
        builder.assign(record, RValue::Record(vec![("x".to_string(), one)]));
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldNamed {
                obj: record,
                field: "x".to_string(),
            },
        );

        let function = finish(builder, Some(projected));
        let candidates = scalar_replacement_candidates(&function);

        assert!(candidates.iter().any(|site| site.dst == record));
    }

    #[test]
    fn mutable_record_stays_local_but_is_not_initial_scalar_candidate() {
        let mut builder = FunctionBuilder::new("mutable_record", Some(Type::int()));
        let one = builder.add_temp(Type::int());
        builder.assign(one, RValue::Const(Constant::Int(1)));
        let two = builder.add_temp(Type::int());
        builder.assign(two, RValue::Const(Constant::Int(2)));

        let record = builder.add_temp(Type::unit());
        builder.assign(record, RValue::Record(vec![("x".to_string(), one)]));
        builder.emit(Stmt::StoreFieldNamed {
            obj: record,
            field: "x".to_string(),
            src: two,
        });
        let projected = builder.add_temp(Type::int());
        builder.assign(
            projected,
            RValue::LoadFieldNamed {
                obj: record,
                field: "x".to_string(),
            },
        );

        let function = finish(builder, Some(projected));
        let summaries = analyze_function(&function);
        let summary = summaries
            .iter()
            .find(|summary| summary.site.dst == record)
            .unwrap();

        assert!(!summary.escapes);
        assert!(!summary.scalar_replaceable);
    }
}
