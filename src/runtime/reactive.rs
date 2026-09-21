//! Reactive query dependency tracking.
//!
//! A reactive query records the exact actor-state fields it reads together
//! with the field revision observed at the first read. The runtime can later
//! invalidate only subscriptions whose dependencies changed instead of
//! broadcasting every actor mutation.
//!
//! Tracking is runtime-scoped rather than actor-scoped so nested workflow
//! queries can contribute dependencies from multiple actors to the outer
//! query. Scopes are stackable; every read is recorded in every active scope,
//! which makes an outer query depend on fields read by nested queries.

use std::cell::RefCell;
use std::collections::BTreeMap;

/// The state fields observed while evaluating one query.
///
/// Keys are `(actor_id, field_name)`; values are the field revision observed
/// at the first read. First-read semantics deliberately make a result stale if
/// a handler mutates a field after reading it, even if it later reads the field
/// again. Query handlers are intended to be read-only, but this keeps the
/// dependency primitive conservative until purity is enforced statically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateReadSet {
    dependencies: BTreeMap<(u64, String), u64>,
}

impl StateReadSet {
    /// Number of distinct actor-field dependencies.
    pub fn len(&self) -> usize {
        self.dependencies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dependencies.is_empty()
    }

    /// Revision observed for one actor-state field.
    pub fn revision(&self, actor_id: u64, field: &str) -> Option<u64> {
        self.dependencies
            .get(&(actor_id, field.to_string()))
            .copied()
    }

    /// Iterate dependencies in deterministic actor/field order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &str, u64)> + '_ {
        self.dependencies
            .iter()
            .map(|((actor_id, field), revision)| (*actor_id, field.as_str(), *revision))
    }

    /// True when a changed field revision invalidates this read set.
    ///
    /// Writes to fields that were not read do not invalidate the query.
    pub fn is_invalidated_by(&self, actor_id: u64, field: &str, revision: u64) -> bool {
        self.revision(actor_id, field)
            .map(|observed| observed != revision)
            .unwrap_or(false)
    }

    /// Check every dependency against a revision lookup.
    ///
    /// Missing actors/fields are stale: disappearance is itself a dependency
    /// change and must not leave a cached query result looking current.
    pub fn is_current_with<F>(&self, mut revision_for: F) -> bool
    where
        F: FnMut(u64, &str) -> Option<u64>,
    {
        self.iter().all(|(actor_id, field, observed)| {
            revision_for(actor_id, field)
                .map(|current| current == observed)
                .unwrap_or(false)
        })
    }

    pub(crate) fn record(&mut self, actor_id: u64, field: &str, revision: u64) {
        self.dependencies
            .entry((actor_id, field.to_string()))
            .or_insert(revision);
    }
}

/// Stackable tracker used by the runtime while evaluating queries.
///
/// `RefCell` lets VM read callbacks record dependencies through an immutable
/// runtime borrow. Runtime shards are single-owner execution contexts; this is
/// not a cross-thread synchronization primitive.
#[derive(Debug, Default)]
pub(crate) struct ReactiveReadTracker {
    scopes: RefCell<Vec<StateReadSet>>,
}

impl ReactiveReadTracker {
    pub(crate) fn begin(&self) {
        self.scopes.borrow_mut().push(StateReadSet::default());
    }

    /// Record a read in every active scope.
    ///
    /// Recording in all scopes is what makes nested queries transparent to an
    /// outer subscription: if A queries B and B reads `B.status`, A's read set
    /// also contains that dependency.
    pub(crate) fn record(&self, actor_id: u64, field: &str, revision: u64) {
        for scope in self.scopes.borrow_mut().iter_mut() {
            scope.record(actor_id, field, revision);
        }
    }

    pub(crate) fn finish(&self) -> Option<StateReadSet> {
        self.scopes.borrow_mut().pop()
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.scopes.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_distinct_fields_and_keeps_first_observed_revision() {
        let tracker = ReactiveReadTracker::default();
        tracker.begin();
        tracker.record(7, "name", 3);
        tracker.record(7, "name", 4);
        tracker.record(7, "status", 9);

        let reads = tracker.finish().unwrap();
        assert_eq!(reads.len(), 2);
        assert_eq!(reads.revision(7, "name"), Some(3));
        assert_eq!(reads.revision(7, "status"), Some(9));
    }

    #[test]
    fn nested_reads_are_inherited_by_outer_scope() {
        let tracker = ReactiveReadTracker::default();
        tracker.begin();
        tracker.record(1, "local", 1);

        tracker.begin();
        tracker.record(2, "remote", 5);
        let inner = tracker.finish().unwrap();

        tracker.record(1, "after", 2);
        let outer = tracker.finish().unwrap();

        assert_eq!(tracker.depth(), 0);
        assert_eq!(inner.revision(2, "remote"), Some(5));
        assert_eq!(inner.revision(1, "local"), None);

        assert_eq!(outer.revision(1, "local"), Some(1));
        assert_eq!(outer.revision(2, "remote"), Some(5));
        assert_eq!(outer.revision(1, "after"), Some(2));
    }

    #[test]
    fn invalidation_is_field_precise() {
        let mut reads = StateReadSet::default();
        reads.record(9, "title", 4);

        assert!(!reads.is_invalidated_by(9, "other", 99));
        assert!(!reads.is_invalidated_by(9, "title", 4));
        assert!(reads.is_invalidated_by(9, "title", 5));

        assert!(reads.is_current_with(|actor_id, field| {
            (actor_id == 9 && field == "title").then_some(4)
        }));
        assert!(!reads.is_current_with(|_, _| None));
    }
}
