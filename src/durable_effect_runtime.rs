//! Storage-neutral coordination for durable external effects.
//!
//! This module connects the semantic state machine in `durable_effect` to the
//! atomic persistence contract. It intentionally does not choose logical
//! effect identity for callers: the caller must provide a replay-stable
//! `DurableEffectSpec` derived from semantic execution identity.

use crate::durable_effect::{
    DurableEffectId, DurableEffectRecord, DurableEffectRecoveryAction,
    DurableEffectRequestMismatch, DurableEffectSpec,
};
use crate::durable_effect_persistence::DurableEffectPersistenceRecord;
use crate::runtime::{
    DurableCommit, DurableTransition, PersistenceStore, DURABLE_TRANSITION_VERSION,
};
use std::fmt;
use std::io;

/// Runtime-owned decision after durable recovery state has been inspected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableEffectDispatchDecision {
    /// A terminal receipt already exists. Return these bytes and do not call
    /// the provider again.
    ReplayRecordedResult(Vec<u8>),
    /// Dispatch may be retried and duplicates are part of the declared
    /// at-least-once contract.
    DispatchAtLeastOnce { operation_id: DurableEffectId },
    /// Dispatch must reuse this stable operation ID as the provider/backend
    /// deduplication key.
    DispatchWithDeduplication { operation_id: DurableEffectId },
    /// The configured backend owns recovery semantics.
    DelegateToBackend { operation_id: DurableEffectId },
}

#[derive(Debug)]
pub enum DurableEffectRuntimeError {
    Storage(io::Error),
    RequestMismatch(DurableEffectRequestMismatch),
    SpecMismatch {
        effect_id: DurableEffectId,
        expected: DurableEffectSpec,
        actual: DurableEffectSpec,
    },
    MissingPreparedEffect {
        effect_id: DurableEffectId,
    },
}

impl fmt::Display for DurableEffectRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "durable effect storage error: {error}"),
            Self::RequestMismatch(error) => error.fmt(f),
            Self::SpecMismatch {
                effect_id,
                expected,
                actual,
            } => write!(
                f,
                "durable effect {effect_id} specification mismatch: expected {:?}, got {:?}",
                expected, actual
            ),
            Self::MissingPreparedEffect { effect_id } => write!(
                f,
                "durable effect {effect_id} cannot complete because no prepared record exists"
            ),
        }
    }
}

impl std::error::Error for DurableEffectRuntimeError {}

impl From<io::Error> for DurableEffectRuntimeError {
    fn from(error: io::Error) -> Self {
        Self::Storage(error)
    }
}

impl From<DurableEffectRequestMismatch> for DurableEffectRuntimeError {
    fn from(error: DurableEffectRequestMismatch) -> Self {
        Self::RequestMismatch(error)
    }
}

/// Coordinates one actor/entity/workflow's durable external effects.
///
/// `activation_epoch` is supplied by the owning runtime/placement layer. A
/// stale epoch is rejected by `PersistenceStore::commit_transition`.
pub struct DurableEffectCoordinator<'a> {
    store: &'a mut dyn PersistenceStore,
    actor_id: u64,
    activation_epoch: u64,
}

impl<'a> DurableEffectCoordinator<'a> {
    pub fn new(
        store: &'a mut dyn PersistenceStore,
        actor_id: u64,
        activation_epoch: u64,
    ) -> Self {
        Self {
            store,
            actor_id,
            activation_epoch,
        }
    }

    /// Inspect recovery history and durably prepare a new effect before any
    /// external dispatch.
    ///
    /// If a record already exists, no new transition is written. Request and
    /// specification identity are validated before returning a recovery
    /// decision.
    pub fn begin(
        &mut self,
        spec: DurableEffectSpec,
        request: &[u8],
    ) -> Result<DurableEffectDispatchDecision, DurableEffectRuntimeError> {
        if let Some(existing) = self.store.load_durable_effect(self.actor_id, spec.id)? {
            self.validate_existing(&spec, request, existing.effect())?;
            return Ok(decision(existing.effect()));
        }

        let prepared = DurableEffectRecord::prepare(spec, request);
        self.commit_record(DurableEffectPersistenceRecord::from_effect(
            prepared.clone(),
        ))?;
        Ok(decision(&prepared))
    }

    /// Persist the terminal result for a previously prepared effect.
    ///
    /// Completion is monotonic. If recovery already committed a terminal
    /// result, the original durable bytes are returned and no new transition
    /// is written.
    pub fn complete(
        &mut self,
        effect_id: DurableEffectId,
        request: &[u8],
        result: Vec<u8>,
    ) -> Result<Vec<u8>, DurableEffectRuntimeError> {
        let Some(existing) = self.store.load_durable_effect(self.actor_id, effect_id)? else {
            return Err(DurableEffectRuntimeError::MissingPreparedEffect { effect_id });
        };

        existing.effect().validate_request(request)?;

        if let DurableEffectRecoveryAction::ReplayRecordedResult(recorded) =
            existing.effect().recovery_action()
        {
            return Ok(recorded.to_vec());
        }

        let completed = existing.effect().clone().complete(result);
        let durable_result = match &completed {
            DurableEffectRecord::Completed { result, .. } => result.clone(),
            DurableEffectRecord::Prepared { .. } => unreachable!("completion must be terminal"),
        };
        self.commit_record(DurableEffectPersistenceRecord::from_effect(completed))?;
        Ok(durable_result)
    }

    fn validate_existing(
        &self,
        expected: &DurableEffectSpec,
        request: &[u8],
        existing: &DurableEffectRecord,
    ) -> Result<(), DurableEffectRuntimeError> {
        if existing.spec() != expected {
            return Err(DurableEffectRuntimeError::SpecMismatch {
                effect_id: expected.id,
                expected: expected.clone(),
                actual: existing.spec().clone(),
            });
        }
        existing.validate_request(request)?;
        Ok(())
    }

    fn commit_record(
        &mut self,
        record: DurableEffectPersistenceRecord,
    ) -> Result<DurableCommit, DurableEffectRuntimeError> {
        let previous = self.store.latest_sequence(self.actor_id);
        let sequence = previous.checked_add(1).ok_or_else(|| {
            DurableEffectRuntimeError::Storage(io::Error::new(
                io::ErrorKind::InvalidData,
                "durable effect transition sequence overflow",
            ))
        })?;
        let transition = DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: self.actor_id,
            activation_epoch: self.activation_epoch,
            sequence,
            expected_previous_sequence: previous,
            command: None,
            snapshot: None,
            workflow_events: Vec::new(),
            domain_events: Vec::new(),
            durable_effects: vec![record],
            outbox: Vec::new(),
        };
        Ok(self.store.commit_transition(transition)?)
    }
}

fn decision(record: &DurableEffectRecord) -> DurableEffectDispatchDecision {
    match record.recovery_action() {
        DurableEffectRecoveryAction::ReplayRecordedResult(result) => {
            DurableEffectDispatchDecision::ReplayRecordedResult(result.to_vec())
        }
        DurableEffectRecoveryAction::RetryAtLeastOnce { operation_id } => {
            DurableEffectDispatchDecision::DispatchAtLeastOnce { operation_id }
        }
        DurableEffectRecoveryAction::RetryWithDeduplication { operation_id } => {
            DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id }
        }
        DurableEffectRecoveryAction::DelegateToBackend => {
            DurableEffectDispatchDecision::DelegateToBackend {
                operation_id: record.spec().id,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{DeliverySemantics, EffectBoundary};
    use crate::runtime::MemoryStore;

    fn spec(actor_id: u64, key: &str, delivery: DeliverySemantics) -> DurableEffectSpec {
        let id = DurableEffectId::derive(actor_id, key, 0, "Provider.ask");
        DurableEffectSpec::new(id, "Provider.ask", EffectBoundary::External, delivery)
    }

    #[test]
    fn begin_persists_prepared_before_dispatch_and_reuses_it_on_recovery() {
        let mut store = MemoryStore::new();
        let expected = spec(42, "workflow:research:step:lookup", DeliverySemantics::AtLeastOnce);
        let id = expected.id;

        {
            let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
            assert_eq!(
                coordinator.begin(expected.clone(), b"prompt").unwrap(),
                DurableEffectDispatchDecision::DispatchAtLeastOnce { operation_id: id }
            );
        }

        assert_eq!(store.latest_sequence(42), 1);
        assert!(matches!(
            store.load_durable_effect(42, id).unwrap().unwrap().effect(),
            DurableEffectRecord::Prepared { .. }
        ));

        {
            let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
            assert_eq!(
                coordinator.begin(expected, b"prompt").unwrap(),
                DurableEffectDispatchDecision::DispatchAtLeastOnce { operation_id: id }
            );
        }

        // Recovery observes the existing intent rather than creating another
        // transition.
        assert_eq!(store.latest_sequence(42), 1);
    }

    #[test]
    fn completed_effect_replays_without_redispatch() {
        let mut store = MemoryStore::new();
        let expected = spec(
            42,
            "workflow:research:step:lookup",
            DeliverySemantics::EffectivelyOnceWithDeduplication,
        );
        let id = expected.id;

        {
            let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
            coordinator.begin(expected.clone(), b"prompt").unwrap();
            assert_eq!(
                coordinator
                    .complete(id, b"prompt", b"answer".to_vec())
                    .unwrap(),
                b"answer"
            );
        }

        assert_eq!(store.latest_sequence(42), 2);

        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert_eq!(
            coordinator.begin(expected, b"prompt").unwrap(),
            DurableEffectDispatchDecision::ReplayRecordedResult(b"answer".to_vec())
        );
        assert_eq!(store.latest_sequence(42), 2);
    }

    #[test]
    fn completion_is_monotonic_when_duplicate_result_arrives() {
        let mut store = MemoryStore::new();
        let expected = spec(42, "turn:7:site:a", DeliverySemantics::AtLeastOnce);
        let id = expected.id;

        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        coordinator.begin(expected, b"prompt").unwrap();
        coordinator
            .complete(id, b"prompt", b"first".to_vec())
            .unwrap();
        assert_eq!(
            coordinator
                .complete(id, b"prompt", b"different-late-result".to_vec())
                .unwrap(),
            b"first"
        );
        assert_eq!(store.latest_sequence(42), 2);
    }

    #[test]
    fn request_drift_fails_closed_without_advancing_history() {
        let mut store = MemoryStore::new();
        let expected = spec(42, "turn:7:site:a", DeliverySemantics::AtLeastOnce);

        {
            let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
            coordinator.begin(expected.clone(), b"prompt-a").unwrap();
        }

        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert!(matches!(
            coordinator.begin(expected, b"prompt-b"),
            Err(DurableEffectRuntimeError::RequestMismatch(_))
        ));
        assert_eq!(store.latest_sequence(42), 1);
    }

    #[test]
    fn stale_activation_cannot_prepare_new_effect() {
        let mut store = MemoryStore::new();

        {
            let mut current = DurableEffectCoordinator::new(&mut store, 42, 2);
            current
                .begin(
                    spec(42, "turn:1", DeliverySemantics::AtLeastOnce),
                    b"first",
                )
                .unwrap();
        }

        let mut stale = DurableEffectCoordinator::new(&mut store, 42, 1);
        let error = stale
            .begin(
                spec(42, "turn:2", DeliverySemantics::AtLeastOnce),
                b"second",
            )
            .unwrap_err();

        assert!(matches!(
            error,
            DurableEffectRuntimeError::Storage(ref source)
                if source.kind() == io::ErrorKind::PermissionDenied
        ));
        assert_eq!(store.latest_sequence(42), 1);
    }
}
