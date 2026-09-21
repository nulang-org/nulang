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

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

/// Runtime-local identity of one actor-state field value.
///
/// `incarnation` changes whenever an Actor instance is reconstructed, while
/// `revision` changes on each write to the field. Together they prevent a
/// read set from becoming accidentally current again after actor replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateVersion {
    pub incarnation: u64,
    pub revision: u64,
}

/// Conservative version for an actor whose pointer-backed state was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActorTurnVersion {
    pub incarnation: u64,
    pub turn_revision: u64,
}

/// The state fields observed while evaluating one query.
///
/// Dependencies are grouped by actor id and field name; each value is the
/// actor incarnation plus field revision observed at the first read.
/// First-read semantics deliberately make a result stale if
/// a handler mutates a field after reading it, even if it later reads the field
/// again. Query handlers are intended to be read-only, but this keeps the
/// dependency primitive conservative until purity is enforced statically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateReadSet {
    dependencies: BTreeMap<u64, BTreeMap<String, StateVersion>>,
    pointer_actor_turns: BTreeMap<u64, ActorTurnVersion>,
}

impl StateReadSet {
    /// Number of distinct actor-field dependencies.
    pub fn len(&self) -> usize {
        self.dependencies.values().map(|fields| fields.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.dependencies.values().all(|fields| fields.is_empty())
    }

    /// Version observed for one actor-state field.
    pub fn version(&self, actor_id: u64, field: &str) -> Option<StateVersion> {
        self.dependencies
            .get(&actor_id)
            .and_then(|fields| fields.get(field))
            .copied()
    }

    /// Field revision observed for one actor-state field.
    pub fn revision(&self, actor_id: u64, field: &str) -> Option<u64> {
        self.version(actor_id, field).map(|version| version.revision)
    }

    /// Iterate dependencies in deterministic actor/field order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &str, StateVersion)> + '_ {
        self.dependencies.iter().flat_map(|(actor_id, fields)| {
            fields
                .iter()
                .map(move |(field, version)| (*actor_id, field.as_str(), *version))
        })
    }

    /// Actor-turn dependency recorded when a query observed pointer-backed
    /// state on `actor_id`.
    pub fn pointer_turn_version(&self, actor_id: u64) -> Option<ActorTurnVersion> {
        self.pointer_actor_turns.get(&actor_id).copied()
    }

    pub fn pointer_turn_iter(
        &self,
    ) -> impl Iterator<Item = (u64, ActorTurnVersion)> + '_ {
        self.pointer_actor_turns
            .iter()
            .map(|(actor_id, version)| (*actor_id, *version))
    }

    /// True when a changed field version invalidates this read set.
    ///
    /// Writes to fields that were not read do not invalidate the query.
    pub fn is_invalidated_by(
        &self,
        actor_id: u64,
        field: &str,
        version: StateVersion,
    ) -> bool {
        self.version(actor_id, field)
            .map(|observed| observed != version)
            .unwrap_or(false)
    }

    /// Check every dependency against a version lookup.
    ///
    /// Missing actors/fields are stale: disappearance is itself a dependency
    /// change and must not leave a cached query result looking current.
    pub fn is_current_with<F, T>(&self, mut version_for: F, mut turn_for: T) -> bool
    where
        F: FnMut(u64, &str) -> Option<StateVersion>,
        T: FnMut(u64) -> Option<ActorTurnVersion>,
    {
        let fields_current = self.iter().all(|(actor_id, field, observed)| {
            version_for(actor_id, field)
                .map(|current| current == observed)
                .unwrap_or(false)
        });
        fields_current
            && self.pointer_turn_iter().all(|(actor_id, observed)| {
                turn_for(actor_id)
                    .map(|current| current == observed)
                    .unwrap_or(false)
            })
    }

    pub(crate) fn record(&mut self, actor_id: u64, field: &str, version: StateVersion) {
        self.dependencies
            .entry(actor_id)
            .or_default()
            .entry(field.to_string())
            .or_insert(version);
    }

    pub(crate) fn record_pointer_turn(
        &mut self,
        actor_id: u64,
        version: ActorTurnVersion,
    ) {
        self.pointer_actor_turns.entry(actor_id).or_insert(version);
    }
}

/// Stable process-local identifier for a reactive workflow-query subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubscriptionId(pub u64);

#[derive(Debug, Clone)]
pub(crate) struct ReactiveSubscription {
    pub actor_id: u64,
    pub query_name: String,
    pub reads: StateReadSet,
}

/// In-process subscription registry with reverse dependency indexes.
///
/// Values are deliberately not retained here: VM values may contain actor-heap
/// pointers. A subscriber receives the initial value directly and refreshes
/// after an invalidation, keeping ownership at the query call boundary.
#[derive(Debug, Default)]
pub(crate) struct ReactiveSubscriptionRegistry {
    next_id: u64,
    subscriptions: BTreeMap<SubscriptionId, ReactiveSubscription>,
    field_index: BTreeMap<(u64, String), BTreeSet<SubscriptionId>>,
    pointer_turn_index: BTreeMap<u64, BTreeSet<SubscriptionId>>,
}

impl ReactiveSubscriptionRegistry {
    pub(crate) fn register(
        &mut self,
        actor_id: u64,
        query_name: String,
        reads: StateReadSet,
    ) -> SubscriptionId {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        let id = SubscriptionId(self.next_id);
        self.index(id, &reads);
        self.subscriptions.insert(
            id,
            ReactiveSubscription {
                actor_id,
                query_name,
                reads,
            },
        );
        id
    }

    pub(crate) fn remove(&mut self, id: SubscriptionId) -> bool {
        let Some(subscription) = self.subscriptions.remove(&id) else {
            return false;
        };
        self.unindex(id, &subscription.reads);
        true
    }

    pub(crate) fn get(&self, id: SubscriptionId) -> Option<&ReactiveSubscription> {
        self.subscriptions.get(&id)
    }

    pub(crate) fn replace_reads(
        &mut self,
        id: SubscriptionId,
        reads: StateReadSet,
    ) -> bool {
        let Some(old_reads) = self
            .subscriptions
            .get(&id)
            .map(|subscription| subscription.reads.clone())
        else {
            return false;
        };
        self.unindex(id, &old_reads);
        self.index(id, &reads);
        if let Some(subscription) = self.subscriptions.get_mut(&id) {
            subscription.reads = reads;
            true
        } else {
            false
        }
    }

    pub(crate) fn candidates(
        &self,
        actor_id: u64,
        dirty_fields: impl IntoIterator<Item = String>,
        turn_changed: bool,
    ) -> BTreeSet<SubscriptionId> {
        let mut candidates = BTreeSet::new();
        for field in dirty_fields {
            if let Some(ids) = self.field_index.get(&(actor_id, field)) {
                candidates.extend(ids.iter().copied());
            }
        }
        if turn_changed {
            if let Some(ids) = self.pointer_turn_index.get(&actor_id) {
                candidates.extend(ids.iter().copied());
            }
        }
        candidates
    }

    fn index(&mut self, id: SubscriptionId, reads: &StateReadSet) {
        for (actor_id, field, _) in reads.iter() {
            self.field_index
                .entry((actor_id, field.to_string()))
                .or_default()
                .insert(id);
        }
        for (actor_id, _) in reads.pointer_turn_iter() {
            self.pointer_turn_index
                .entry(actor_id)
                .or_default()
                .insert(id);
        }
    }

    fn unindex(&mut self, id: SubscriptionId, reads: &StateReadSet) {
        for (actor_id, field, _) in reads.iter() {
            let key = (actor_id, field.to_string());
            let empty = self
                .field_index
                .get_mut(&key)
                .map(|ids| {
                    ids.remove(&id);
                    ids.is_empty()
                })
                .unwrap_or(false);
            if empty {
                self.field_index.remove(&key);
            }
        }
        for (actor_id, _) in reads.pointer_turn_iter() {
            let empty = self
                .pointer_turn_index
                .get_mut(&actor_id)
                .map(|ids| {
                    ids.remove(&id);
                    ids.is_empty()
                })
                .unwrap_or(false);
            if empty {
                self.pointer_turn_index.remove(&actor_id);
            }
        }
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
    /// Fast-path depth check used by every StateGet. Keeping this separate
    /// avoids borrowing the scope vector when no query is being tracked.
    active_depth: Cell<usize>,
}

impl ReactiveReadTracker {
    pub(crate) fn begin(&self) {
        self.scopes.borrow_mut().push(StateReadSet::default());
        self.active_depth.set(self.active_depth.get() + 1);
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.active_depth.get() != 0
    }

    /// Record a read in every active scope.
    ///
    /// Recording in all scopes is what makes nested queries transparent to an
    /// outer subscription: if A queries B and B reads `B.status`, A's read set
    /// also contains that dependency.
    pub(crate) fn record(&self, actor_id: u64, field: &str, version: StateVersion) {
        debug_assert!(self.is_active());
        for scope in self.scopes.borrow_mut().iter_mut() {
            scope.record(actor_id, field, version);
        }
    }

    pub(crate) fn record_pointer_turn(
        &self,
        actor_id: u64,
        version: ActorTurnVersion,
    ) {
        debug_assert!(self.is_active());
        for scope in self.scopes.borrow_mut().iter_mut() {
            scope.record_pointer_turn(actor_id, version);
        }
    }

    pub(crate) fn finish(&self) -> Option<StateReadSet> {
        let mut scopes = self.scopes.borrow_mut();
        let result = scopes.pop();
        self.active_depth.set(scopes.len());
        result
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.active_depth.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_distinct_fields_and_keeps_first_observed_revision() {
        let tracker = ReactiveReadTracker::default();
        tracker.begin();
        tracker.record(
            7,
            "name",
            StateVersion {
                incarnation: 2,
                revision: 3,
            },
        );
        tracker.record(
            7,
            "name",
            StateVersion {
                incarnation: 2,
                revision: 4,
            },
        );
        tracker.record(
            7,
            "status",
            StateVersion {
                incarnation: 2,
                revision: 9,
            },
        );

        let reads = tracker.finish().unwrap();
        assert_eq!(reads.len(), 2);
        assert_eq!(reads.revision(7, "name"), Some(3));
        assert_eq!(reads.revision(7, "status"), Some(9));
    }

    #[test]
    fn nested_reads_are_inherited_by_outer_scope() {
        let tracker = ReactiveReadTracker::default();
        tracker.begin();
        tracker.record(
            1,
            "local",
            StateVersion {
                incarnation: 10,
                revision: 1,
            },
        );

        tracker.begin();
        tracker.record(
            2,
            "remote",
            StateVersion {
                incarnation: 20,
                revision: 5,
            },
        );
        let inner = tracker.finish().unwrap();

        tracker.record(
            1,
            "after",
            StateVersion {
                incarnation: 10,
                revision: 2,
            },
        );
        let outer = tracker.finish().unwrap();

        assert_eq!(tracker.depth(), 0);
        assert_eq!(inner.revision(2, "remote"), Some(5));
        assert_eq!(inner.revision(1, "local"), None);

        assert_eq!(outer.revision(1, "local"), Some(1));
        assert_eq!(outer.revision(2, "remote"), Some(5));
        assert_eq!(outer.revision(1, "after"), Some(2));
    }

    #[test]
    fn runtime_tracking_is_field_precise_and_rejects_actor_replacement() {
        use crate::runtime::{Actor, Runtime};
        use crate::vm::Value;

        let mut rt = Runtime::new();
        let mut actor = Actor::new(42, "query-target", 8);
        actor.set_state_field("title", Value::int(1));
        actor.set_state_field("other", Value::int(1));
        rt.actors.insert(42, actor);

        rt.begin_reactive_query_tracking();
        rt.record_reactive_state_read(42, "title");
        let reads = rt.finish_reactive_query_tracking().unwrap();

        assert!(rt.state_read_set_is_current(&reads));

        rt.actors
            .get_mut(&42)
            .unwrap()
            .set_state_field("other", Value::int(2));
        assert!(rt.state_read_set_is_current(&reads));

        rt.actors
            .get_mut(&42)
            .unwrap()
            .set_state_field("title", Value::int(2));
        assert!(!rt.state_read_set_is_current(&reads));

        rt.begin_reactive_query_tracking();
        rt.record_reactive_state_read(42, "title");
        let before_restart = rt.finish_reactive_query_tracking().unwrap();
        assert!(rt.state_read_set_is_current(&before_restart));

        let mut replacement = Actor::new(42, "query-target", 8);
        replacement.set_state_field("title", Value::int(2));
        rt.actors.insert(42, replacement);

        assert!(
            !rt.state_read_set_is_current(&before_restart),
            "a fresh actor incarnation must invalidate read sets from the old instance"
        );
    }

    #[test]
    fn pointer_turn_dependency_invalidates_on_later_actor_turn() {
        let mut reads = StateReadSet::default();
        let turn = ActorTurnVersion {
            incarnation: 11,
            turn_revision: 4,
        };
        reads.record_pointer_turn(7, turn);

        assert_eq!(reads.pointer_turn_version(7), Some(turn));
        assert!(reads.is_current_with(
            |_, _| None,
            |actor_id| (actor_id == 7).then_some(turn),
        ));
        assert!(!reads.is_current_with(
            |_, _| None,
            |actor_id| {
                (actor_id == 7).then_some(ActorTurnVersion {
                    incarnation: 11,
                    turn_revision: 5,
                })
            },
        ));
    }

    #[test]
    fn pointer_state_reads_depend_on_actor_turn_revision() {
        use crate::runtime::{Actor, Runtime};
        use crate::vm::Value;

        let mut rt = Runtime::new();
        let mut actor = Actor::new(55, "pointer-query-target", 8);
        let pointer = actor.allocate_string("hello");
        assert!(pointer.as_ptr().is_some());
        actor.set_state_field("payload", pointer);
        rt.actors.insert(55, actor);

        rt.begin_reactive_query_tracking();
        rt.record_reactive_state_read(55, "payload");
        let reads = rt.finish_reactive_query_tracking().unwrap();

        assert!(reads.pointer_turn_version(55).is_some());
        assert!(rt.state_read_set_is_current(&reads));

        rt.actors.get_mut(&55).unwrap().begin_reactive_turn();
        assert!(
            !rt.state_read_set_is_current(&reads),
            "a later actor turn must conservatively invalidate pointer-backed reads"
        );
    }

    #[test]
    fn subscription_registry_indexes_exact_fields_and_pointer_turns() {
        let mut registry = ReactiveSubscriptionRegistry::default();

        let mut scalar_reads = StateReadSet::default();
        scalar_reads.record(
            1,
            "name",
            StateVersion {
                incarnation: 1,
                revision: 2,
            },
        );
        let scalar = registry.register(1, "scalar".into(), scalar_reads);

        let mut pointer_reads = StateReadSet::default();
        pointer_reads.record(
            2,
            "payload",
            StateVersion {
                incarnation: 2,
                revision: 1,
            },
        );
        pointer_reads.record_pointer_turn(
            2,
            ActorTurnVersion {
                incarnation: 2,
                turn_revision: 9,
            },
        );
        let pointer = registry.register(2, "pointer".into(), pointer_reads);

        assert_eq!(
            registry.candidates(1, ["other".to_string()], false),
            BTreeSet::new()
        );
        assert_eq!(
            registry.candidates(1, ["name".to_string()], false),
            BTreeSet::from([scalar])
        );
        assert_eq!(
            registry.candidates(2, std::iter::empty::<String>(), true),
            BTreeSet::from([pointer])
        );

        assert!(registry.remove(scalar));
        assert!(registry
            .candidates(1, ["name".to_string()], false)
            .is_empty());
    }

    #[test]
    fn invalidation_is_field_precise() {
        let mut reads = StateReadSet::default();
        let observed = StateVersion {
            incarnation: 4,
            revision: 4,
        };
        reads.record(9, "title", observed);

        assert!(!reads.is_invalidated_by(
            9,
            "other",
            StateVersion {
                incarnation: 4,
                revision: 99,
            },
        ));
        assert!(!reads.is_invalidated_by(9, "title", observed));
        assert!(reads.is_invalidated_by(
            9,
            "title",
            StateVersion {
                incarnation: 4,
                revision: 5,
            },
        ));
        assert!(reads.is_invalidated_by(
            9,
            "title",
            StateVersion {
                incarnation: 5,
                revision: 4,
            },
        ));

        assert!(reads.is_current_with(
            |actor_id, field| (actor_id == 9 && field == "title").then_some(observed),
            |_| None,
        ));
        assert!(!reads.is_current_with(|_, _| None, |_| None));
    }
}
