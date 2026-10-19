//! Conservative MIR access facts and a scoped-exclusivity prototype.
//!
//! This module is intentionally **not** a source-language borrow checker.
//! MIR `Load` is not evidence of a move; two MIR locals are not proven
//! non-aliasing merely because their register ids differ. The facts here are
//! intended for diagnostics and future ownership-aware MIR passes, never as
//! justification to remove a copy/retain or relax a capability check.
//!
//! The explicit event validator is independent of lowering. It can validate
//! scopes supplied by a future frontend once their provenance is established.

use crate::mir::{BlockId, FuncRef, Function, LocalId, RValue, Stmt, Terminator};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Modify,
    Consume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    Field(String),
    Tuple(u8),
    /// A known constant index or an unknown/dynamic index.
    Index(Option<u64>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPath {
    pub root: LocalId,
    pub projections: Vec<Projection>,
}

impl AccessPath {
    pub fn new(root: LocalId) -> Self {
        Self {
            root,
            projections: Vec::new(),
        }
    }

    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.projections.push(Projection::Field(field.into()));
        self
    }

    pub fn index(mut self, index: Option<u64>) -> Self {
        self.projections.push(Projection::Index(index));
        self
    }
}

/// Returns true unless the projections can be proven disjoint.
///
/// Distinct LocalIds may alias the same heap object. No "different register
/// means different allocation" shortcut is permitted here. Disjoint named
/// fields and known indices are recognizable only under the *same* root.
pub fn may_overlap(a: &AccessPath, b: &AccessPath) -> bool {
    if a.root != b.root {
        return true;
    }
    for (left, right) in a.projections.iter().zip(&b.projections) {
        match (left, right) {
            (Projection::Field(x), Projection::Field(y)) if x != y => return false,
            (Projection::Tuple(x), Projection::Tuple(y)) if x != y => return false,
            (Projection::Index(Some(x)), Projection::Index(Some(y))) if x != y => {
                return false
            }
            _ => {}
        }
    }
    // Equal, prefix-related, dynamic, or otherwise unproven projections.
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Call,
    Effect,
    Suspend,
    ForeignCall,
    Actor,
    ActorState,
    Durable,
    Handler,
    Authority,
    Parallel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessSite {
    pub block: BlockId,
    /// For a terminator, this equals the number of statements in its block.
    pub statement_index: usize,
    pub reads: Vec<AccessPath>,
    pub modifies: Vec<AccessPath>,
    /// Register definitions, not heap mutation or ownership consumption.
    pub defines: Vec<LocalId>,
    pub boundaries: Vec<Boundary>,
}

impl AccessSite {
    fn new(block: BlockId, statement_index: usize) -> Self {
        Self {
            block,
            statement_index,
            reads: Vec::new(),
            modifies: Vec::new(),
            defines: Vec::new(),
            boundaries: Vec::new(),
        }
    }

    fn read(&mut self, local: LocalId) {
        self.reads.push(AccessPath::new(local));
    }

    fn read_all(&mut self, locals: &[LocalId]) {
        for &local in locals {
            self.read(local);
        }
    }

    fn populated(&self) -> bool {
        !self.reads.is_empty()
            || !self.modifies.is_empty()
            || !self.defines.is_empty()
            || !self.boundaries.is_empty()
    }
}

/// Summarize actual MIR reads, aggregate mutations, register definitions,
/// and operations which a future exclusive view cannot blindly cross.
///
/// These are descriptive access facts. In particular, there is deliberately
/// no inferred `Consume` site: MIR loads, calls, and sends do not encode
/// sufficient source-level ownership information to infer a move.
pub fn summarize_function(function: &Function) -> Vec<AccessSite> {
    let mut sites = Vec::new();
    for block in &function.blocks {
        for (index, stmt) in block.stmts.iter().enumerate() {
            let mut site = AccessSite::new(block.id, index);
            match stmt {
                Stmt::Assign { dst, op } => {
                    inspect_rvalue(op, &mut site);
                    site.defines.push(*dst);
                }
                Stmt::StoreFieldNamed { obj, field, src } => {
                    site.modifies.push(AccessPath::new(*obj).field(field));
                    site.read(*src);
                }
                Stmt::ArrayStore { arr, idx, src } => {
                    site.modifies.push(AccessPath::new(*arr).index(None));
                    site.read(*idx);
                    site.read(*src);
                }
                Stmt::EnterHandle { .. } | Stmt::PopHandler => {
                    site.boundaries.push(Boundary::Handler);
                }
                Stmt::Emit { args, .. } => {
                    site.read_all(args);
                    site.boundaries.push(Boundary::Durable);
                }
                Stmt::StateSet { src, .. } => {
                    site.read(*src);
                    site.boundaries.push(Boundary::ActorState);
                }
                Stmt::ParallelMarker { .. } => {
                    site.boundaries.push(Boundary::Parallel);
                }
            }
            if site.populated() {
                sites.push(site);
            }
        }
        let mut site = AccessSite::new(block.id, block.stmts.len());
        match &block.terminator {
            Terminator::Return(Some(value)) | Terminator::Resume(value) => {
                site.read(*value);
                if matches!(block.terminator, Terminator::Resume(_)) {
                    site.boundaries.push(Boundary::Handler);
                }
            }
            Terminator::Branch { cond, .. } => site.read(*cond),
            Terminator::Return(None)
            | Terminator::Jump(_)
            | Terminator::Unterminated => {}
        }
        if site.populated() {
            sites.push(site);
        }
    }
    sites
}

fn inspect_rvalue(op: &RValue, site: &mut AccessSite) {
    match op {
        RValue::Const(_) | RValue::Panic(_) | RValue::SelfRef | RValue::ReceiveCommit => {}
        RValue::Load(value)
        | RValue::ArrayLen(value)
        | RValue::Unary(_, value) => site.read(*value),
        RValue::LoadFieldNamed { obj, field } => {
            site.reads.push(AccessPath::new(*obj).field(field));
        }
        RValue::LoadFieldPos { obj, index } => {
            let mut path = AccessPath::new(*obj);
            path.projections.push(Projection::Tuple(*index));
            site.reads.push(path);
        }
        RValue::ArrayLoad { arr, idx } => {
            site.reads.push(AccessPath::new(*arr).index(None));
            site.read(*idx);
        }
        RValue::ArrayLit(values) | RValue::Tuple(values) => site.read_all(values),
        RValue::Binary(_, a, b) | RValue::StringEq(a, b) | RValue::StrConcat(a, b) => {
            site.read(*a);
            site.read(*b);
        }
        RValue::Call { func, args } => {
            if let FuncRef::Local(local) = func {
                site.read(*local);
            }
            site.read_all(args);
            site.boundaries.push(Boundary::Call);
        }
        RValue::Closure { captures, .. } => site.read_all(captures),
        RValue::Record(fields) => {
            for (_, value) in fields {
                site.read(*value);
            }
        }
        RValue::RecordUpdate { base, overrides } => {
            site.read(*base);
            for (_, value) in overrides {
                site.read(*value);
            }
        }
        RValue::Perform { args, .. } => {
            site.read_all(args);
            site.boundaries.push(Boundary::Effect);
        }
        RValue::PerformAsync { args, .. } => {
            site.read_all(args);
            site.boundaries.push(Boundary::Effect);
            site.boundaries.push(Boundary::Suspend);
        }
        RValue::SignalWait { .. } => site.boundaries.push(Boundary::Suspend),
        RValue::Receive | RValue::ReceiveMatch { .. } => {
            site.boundaries.push(Boundary::Actor);
        }
        RValue::ReceiveWait { timeout, .. } => {
            site.read(*timeout);
            site.boundaries.push(Boundary::Actor);
            site.boundaries.push(Boundary::Suspend);
        }
        RValue::FFICall { args, .. } => {
            site.read_all(args);
            site.boundaries.push(Boundary::ForeignCall);
        }
        RValue::Migrate { actor, node } => {
            site.read(*actor);
            site.read(*node);
            site.boundaries.push(Boundary::Actor);
        }
        RValue::CapabilityCheck { val } => {
            site.read(*val);
            site.boundaries.push(Boundary::Authority);
        }
        RValue::StateGet { .. } => site.boundaries.push(Boundary::ActorState),
        RValue::Spawn {
            init, target_node, ..
        } => {
            for (_, value) in init {
                inspect_rvalue(value, site);
            }
            if let Some(node) = target_node {
                site.read(*node);
            }
            site.boundaries.push(Boundary::Actor);
        }
        RValue::Send { actor, args, .. } => {
            site.read(*actor);
            site.read_all(args);
            site.boundaries.push(Boundary::Actor);
        }
        RValue::Ask { actor, args, .. } => {
            site.read(*actor);
            site.read_all(args);
            site.boundaries.push(Boundary::Actor);
            site.boundaries.push(Boundary::Suspend);
        }
        RValue::Resume(value) => {
            site.read(*value);
            site.boundaries.push(Boundary::Handler);
        }
    }
}

/// Explicit borrow/consume events must come from a provenance-aware frontend.
/// Do not manufacture these from `RValue::Load`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessEvent {
    Begin { path: AccessPath, kind: AccessKind },
    End { path: AccessPath },
    Consume { path: AccessPath },
    /// Only a complete root redefinition restores access after consumption.
    Reinitialize { root: LocalId },
    Boundary(Boundary),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessErrorKind {
    ConflictingAccess,
    UsedAfterConsume,
    ConsumedDuringBorrow,
    InvalidScopedConsume,
    UnmatchedEnd,
    CrossesBoundary,
    UnterminatedBorrow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessError {
    pub event_index: usize,
    pub kind: AccessErrorKind,
}

/// Check explicitly scoped borrows and consumption, failing closed on alias
/// uncertainty and on *all* unknown/suspending boundaries. This is an
/// experimental checker, not yet invoked by production source lowering.
pub fn validate_scoped_access(events: &[AccessEvent]) -> Result<(), AccessError> {
    let mut active: Vec<(AccessPath, AccessKind)> = Vec::new();
    let mut consumed: Vec<AccessPath> = Vec::new();
    for (event_index, event) in events.iter().enumerate() {
        let fail = |kind| AccessError { event_index, kind };
        match event {
            AccessEvent::Begin { path, kind } => {
                if *kind == AccessKind::Consume {
                    return Err(fail(AccessErrorKind::InvalidScopedConsume));
                }
                if consumed.iter().any(|prior| may_overlap(prior, path)) {
                    return Err(fail(AccessErrorKind::UsedAfterConsume));
                }
                if active.iter().any(|(prior, prior_kind)| {
                    may_overlap(prior, path)
                        && (*kind == AccessKind::Modify || *prior_kind == AccessKind::Modify)
                }) {
                    return Err(fail(AccessErrorKind::ConflictingAccess));
                }
                active.push((path.clone(), *kind));
            }
            AccessEvent::End { path } => {
                if let Some(position) = active.iter().rposition(|(prior, _)| prior == path) {
                    active.remove(position);
                } else {
                    return Err(fail(AccessErrorKind::UnmatchedEnd));
                }
            }
            AccessEvent::Consume { path } => {
                if consumed.iter().any(|prior| may_overlap(prior, path)) {
                    return Err(fail(AccessErrorKind::UsedAfterConsume));
                }
                if active.iter().any(|(prior, _)| may_overlap(prior, path)) {
                    return Err(fail(AccessErrorKind::ConsumedDuringBorrow));
                }
                consumed.push(path.clone());
            }
            AccessEvent::Reinitialize { root } => {
                let path = AccessPath::new(*root);
                if active.iter().any(|(prior, _)| may_overlap(prior, &path)) {
                    return Err(fail(AccessErrorKind::ConflictingAccess));
                }
                consumed.retain(|prior| prior.root != *root);
            }
            AccessEvent::Boundary(_) if !active.is_empty() => {
                return Err(fail(AccessErrorKind::CrossesBoundary));
            }
            AccessEvent::Boundary(_) => {}
        }
    }
    if !active.is_empty() {
        return Err(AccessError {
            event_index: events.len(),
            kind: AccessErrorKind::UnterminatedBorrow,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::FunctionBuilder;
    use crate::types::Type;

    fn p() -> AccessPath {
        AccessPath::new(LocalId(0))
    }

    fn begin(path: AccessPath, kind: AccessKind) -> AccessEvent {
        AccessEvent::Begin { path, kind }
    }

    fn end(path: AccessPath) -> AccessEvent {
        AccessEvent::End { path }
    }

    fn expect_error(events: &[AccessEvent], at: usize, kind: AccessErrorKind) {
        assert_eq!(
            validate_scoped_access(events),
            Err(AccessError {
                event_index: at,
                kind,
            })
        );
    }

    #[test]
    fn overlapping_shared_reads_are_allowed() {
        let root = p();
        assert_eq!(validate_scoped_access(&[
            begin(root.clone(), AccessKind::Read),
            begin(root.clone(), AccessKind::Read),
            end(root.clone()),
            end(root),
        ]), Ok(()));
    }

    #[test]
    fn mutable_access_excludes_overlapping_read() {
        let root = p();
        expect_error(&[
            begin(root.clone(), AccessKind::Modify),
            begin(root, AccessKind::Read),
        ], 1, AccessErrorKind::ConflictingAccess);
    }

    #[test]
    fn distinct_record_fields_can_be_mutated_independently() {
        let left = p().field("left");
        let right = p().field("right");
        assert_eq!(validate_scoped_access(&[
            begin(left.clone(), AccessKind::Modify),
            begin(right.clone(), AccessKind::Modify),
            end(right),
            end(left),
        ]), Ok(()));
    }

    #[test]
    fn known_distinct_indices_are_independent_but_dynamic_indices_are_not() {
        assert!(!may_overlap(&p().index(Some(1)), &p().index(Some(2))));
        assert!(may_overlap(&p().index(None), &p().index(Some(2))));
        expect_error(&[
            begin(p().index(None), AccessKind::Modify),
            begin(p().index(Some(2)), AccessKind::Modify),
        ], 1, AccessErrorKind::ConflictingAccess);
    }

    #[test]
    fn different_registers_are_not_proven_disjoint() {
        let other = AccessPath::new(LocalId(1));
        assert!(may_overlap(&p(), &other));
        expect_error(&[
            begin(p(), AccessKind::Modify),
            begin(other, AccessKind::Read),
        ], 1, AccessErrorKind::ConflictingAccess);
    }

    #[test]
    fn consumption_is_not_a_transient_borrow_and_blocks_reuse() {
        let root = p();
        expect_error(&[
            AccessEvent::Consume { path: root.clone() },
            begin(root, AccessKind::Read),
        ], 1, AccessErrorKind::UsedAfterConsume);
    }

    #[test]
    fn consumption_rejects_live_borrow() {
        let root = p();
        expect_error(&[
            begin(root.clone(), AccessKind::Read),
            AccessEvent::Consume { path: root },
        ], 1, AccessErrorKind::ConsumedDuringBorrow);
    }

    #[test]
    fn root_reinitialization_restores_consumed_value_only_after_lease_ends() {
        let root = p();
        assert_eq!(validate_scoped_access(&[
            AccessEvent::Consume { path: root.clone() },
            AccessEvent::Reinitialize { root: root.root },
            begin(root.clone(), AccessKind::Modify),
            end(root),
        ]), Ok(()));
    }

    #[test]
    fn effect_and_suspend_boundaries_reject_live_borrows() {
        let root = p();
        for boundary in [Boundary::Effect, Boundary::Suspend, Boundary::ForeignCall,
            Boundary::Actor, Boundary::Durable, Boundary::Handler] {
            expect_error(&[
                begin(root.clone(), AccessKind::Read),
                AccessEvent::Boundary(boundary),
            ], 1, AccessErrorKind::CrossesBoundary);
        }
    }

    #[test]
    fn unclosed_and_unmatched_scopes_are_errors() {
        expect_error(&[begin(p(), AccessKind::Read)], 1, AccessErrorKind::UnterminatedBorrow);
        expect_error(&[end(p())], 0, AccessErrorKind::UnmatchedEnd);
    }

    #[test]
    fn mir_summary_distinguishes_field_mutation_from_value_copy_and_effect() {
        let mut builder = FunctionBuilder::new("access_test", None);
        let object = builder.add_local("record", Type::unit());
        let value = builder.add_local("value", Type::unit());
        builder.emit(Stmt::StoreFieldNamed {
            obj: object,
            field: "name".into(),
            src: value,
        });
        builder.assign(value, RValue::Load(object));
        builder.assign(value, RValue::Perform {
            effect: "IO".into(),
            op: "print".into(),
            args: vec![object],
            resolved_handler: None,
        });
        builder.terminate(Terminator::Return(None));
        let sites = summarize_function(&builder.build());
        assert_eq!(sites.len(), 3);
        assert_eq!(sites[0].modifies, vec![AccessPath::new(object).field("name")]);
        assert_eq!(sites[0].reads, vec![AccessPath::new(value)]);
        assert_eq!(sites[1].reads, vec![AccessPath::new(object)]);
        assert_eq!(sites[1].defines, vec![value]);
        assert!(sites[1].boundaries.is_empty());
        assert_eq!(sites[2].boundaries, vec![Boundary::Effect]);
    }

    #[test]
    fn dynamic_array_store_does_not_assert_disjoint_indices() {
        let mut builder = FunctionBuilder::new("array_access", None);
        let arr = builder.add_local("array", Type::unit());
        let idx = builder.add_local("index", Type::unit());
        let val = builder.add_local("value", Type::unit());
        builder.emit(Stmt::ArrayStore { arr, idx, src: val });
        builder.terminate(Terminator::Return(None));
        let sites = summarize_function(&builder.build());
        assert_eq!(sites[0].modifies, vec![AccessPath::new(arr).index(None)]);
        assert_eq!(sites[0].reads, vec![AccessPath::new(idx), AccessPath::new(val)]);
    }
}
