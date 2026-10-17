//! Storage-neutral coordination for durable external effects.
//!
//! This module connects the semantic state machine in `durable_effect` to the
//! atomic persistence contract. It intentionally does not choose logical
//! effect identity for callers: the caller must provide a replay-stable
//! `DurableEffectSpec` derived from semantic execution identity.

use crate::authority::AuthorityGrant;
use crate::authority_runtime::RuntimeAuthorityError;
use crate::bytecode::CodeModule;
use crate::durable_effect::{
    CompiledEffectSpecError, DurableEffectId, DurableEffectRecord, DurableEffectRecoveryAction,
    DurableEffectRequestMismatch, DurableEffectSpec,
};
use crate::primitives::{DeliverySemantics, EffectBoundary};
use crate::durable_effect_persistence::DurableEffectPersistenceRecord;
use crate::runtime::{
    Actor, DurableCommit, DurableTransition, PersistenceStore, DURABLE_TRANSITION_VERSION,
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

/// Failure while dispatching one compiled external effect through a durable
/// intent/receipt boundary. A provider error leaves the prepared record intact
/// because the remote operation may have committed before its reply was lost.
#[derive(Debug)]
pub enum DurableAsyncDispatchError {
    InvalidSite(CompiledEffectSpecError),
    Authorization(RuntimeAuthorityError),
    ActorMismatch { expected: u64, actual: u64 },
    ActorEpochMismatch { expected: u64, actual: u64 },
    GrantOperationMismatch { operation: String, grant: AuthorityGrant },
    Recovery(DurableEffectRuntimeError),
    Provider(io::Error),
    UnexpectedDecision,
}

impl fmt::Display for DurableAsyncDispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSite(error) => error.fmt(f),
            Self::Authorization(error) => error.fmt(f),
            Self::ActorMismatch { expected, actual } => write!(
                f,
                "durable effect actor mismatch: expected {expected}, got {actual}"
            ),
            Self::ActorEpochMismatch { expected, actual } => write!(
                f,
                "durable effect activation epoch mismatch: expected {expected}, got {actual}"
            ),
            Self::GrantOperationMismatch { operation, grant } => write!(
                f,
                "durable effect {operation} cannot be authorized by unrelated or parameterized grant {grant}"
            ),
            Self::Recovery(error) => error.fmt(f),
            Self::Provider(error) => write!(f, "durable effect provider error: {error}"),
            Self::UnexpectedDecision => write!(
                f,
                "compiled idempotent external effect returned incompatible dispatch semantics"
            ),
        }
    }
}

impl std::error::Error for DurableAsyncDispatchError {}

impl From<CompiledEffectSpecError> for DurableAsyncDispatchError {
    fn from(error: CompiledEffectSpecError) -> Self {
        Self::InvalidSite(error)
    }
}

impl From<DurableEffectRuntimeError> for DurableAsyncDispatchError {
    fn from(error: DurableEffectRuntimeError) -> Self {
        Self::Recovery(error)
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
    pub fn new(store: &'a mut dyn PersistenceStore, actor_id: u64, activation_epoch: u64) -> Self {
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
            let recovered = decision(existing.effect());
            // A completed receipt can be read without provider dispatch.
            // Prepared effects, however, must never authorize a stale or
            // unclaimed future activation to retry the external operation.
            if !matches!(recovered, DurableEffectDispatchDecision::ReplayRecordedResult(_)) {
                self.require_claimed_activation()?;
            }
            return Ok(recovered);
        }

        let prepared = DurableEffectRecord::prepare(spec, request);
        self.commit_record(DurableEffectPersistenceRecord::from_effect(
            prepared.clone(),
        ))?;
        Ok(decision(&prepared))
    }

    /// Execute a compiler-identified external operation with an idempotent
    /// provider, journaling Prepared before the provider and Completed after
    /// a successful receipt. A completed effect replays without dispatch.
    ///
    /// The caller MUST supply a verified/pinned CodeModule, a durable and
    /// replay-stable execution key, and the same occurrence index on retry.
    /// The provider MUST honor the supplied stable idempotency key. A provider
    /// timeout or error leaves Prepared intact for a future deduplicated retry.
    ///
    /// Require a live actor with the exact ID, activation epoch and typed
    /// authority for the compiler's effect operation BEFORE journaling, replay,
    /// or provider dispatch. For now this bridge intentionally permits only an
    /// unparameterized Other(Namespace::Operation) grant matching the compiled
    /// Namespace.operation site. Resource-scoped grants need a separate adapter
    /// that validates the actual destination/arguments, not a guessed string.
    ///
    /// This is a trusted-host Rust boundary, not VM integration. The host must
    /// supply the actual live actor and ensure the provider closure implements
    /// the compiled operation. A competing owner can still claim a newer epoch
    /// after this check; provider-side deduplication remains mandatory.
    pub fn execute_compiled_async_with_deduplication<F>(
        &mut self,
        actor: &Actor,
        authority: &AuthorityGrant,
        module: &CodeModule,
        pc: usize,
        execution_key: &str,
        occurrence_index: u32,
        request: &[u8],
        dispatch: F,
    ) -> Result<Vec<u8>, DurableAsyncDispatchError>
    where
        F: FnOnce(&str, &[u8]) -> io::Result<Vec<u8>>,
    {
        if actor.id != self.actor_id {
            return Err(DurableAsyncDispatchError::ActorMismatch {
                expected: self.actor_id,
                actual: actor.id,
            });
        }
        if actor.activation_epoch != self.activation_epoch {
            return Err(DurableAsyncDispatchError::ActorEpochMismatch {
                expected: self.activation_epoch,
                actual: actor.activation_epoch,
            });
        }

        let spec = DurableEffectSpec::from_compiled_async_site(
            module,
            pc,
            self.actor_id,
            execution_key,
            occurrence_index,
            EffectBoundary::External,
            DeliverySemantics::EffectivelyOnceWithDeduplication,
        )?;

        // Never infer a resource-scoped grant from a static call site: a
        // mutable request may name a different path, endpoint, or secret.
        // Until the compiler emits resource-binding descriptors only exact,
        // unparameterized extension operations can use this generic bridge.
        let matches_operation = matches!(
            authority,
            AuthorityGrant::Other { namespace, operation, argument: None }
                if spec.effect_operation.split_once('.')
                    == Some((namespace.as_str(), operation.as_str()))
        );
        if !matches_operation {
            return Err(DurableAsyncDispatchError::GrantOperationMismatch {
                operation: spec.effect_operation.clone(),
                grant: authority.clone(),
            });
        }
        actor
            .require_authority(authority)
            .map_err(DurableAsyncDispatchError::Authorization)?;

        match self.begin(spec, request)? {
            DurableEffectDispatchDecision::ReplayRecordedResult(receipt) => Ok(receipt),
            DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id } => {
                let key = operation_id.idempotency_key();
                let receipt =
                    dispatch(&key, request).map_err(DurableAsyncDispatchError::Provider)?;
                Ok(self.complete(operation_id, request, receipt)?)
            }
            _ => Err(DurableAsyncDispatchError::UnexpectedDecision),
        }
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

    /// A prepared intent may predate an actor's current activation, but
    /// redispatch is allowed only after the owner's new epoch is durably
    /// claimed. This read fence does not replace provider-side deduplication
    /// or prevent a competing claim after the check.
    fn require_claimed_activation(&self) -> Result<(), DurableEffectRuntimeError> {
        let tail = self
            .store
            .load_durable_tail_position(self.actor_id)?
            .ok_or_else(|| {
                DurableEffectRuntimeError::Storage(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "prepared durable effect exists without a committed activation tail",
                ))
            })?;
        if tail.activation_epoch != self.activation_epoch {
            return Err(DurableEffectRuntimeError::Storage(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "durable effect redispatch epoch {} is not the committed owner epoch {}",
                    self.activation_epoch, tail.activation_epoch
                ),
            )));
        }
        Ok(())
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
    use crate::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

    fn spec(actor_id: u64, key: &str, delivery: DeliverySemantics) -> DurableEffectSpec {
        let site = effect_site_id(
            "durable-effect-runtime-tests",
            EffectSiteOwnerKind::Behavior,
            "TestActor.run",
            "Provider.ask",
            0,
        );
        let id = DurableEffectId::derive_from_site(actor_id, key, site, 0);
        DurableEffectSpec::new(id, "Provider.ask", EffectBoundary::External, delivery)
    }

    #[test]
    fn begin_persists_prepared_before_dispatch_and_reuses_it_on_recovery() {
        let mut store = MemoryStore::new();
        let expected = spec(
            42,
            "workflow:research:step:lookup",
            DeliverySemantics::AtLeastOnce,
        );
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
    fn specification_drift_fails_closed_without_advancing_history() {
        let mut store = MemoryStore::new();
        let expected = spec(42, "turn:7:site:a", DeliverySemantics::AtLeastOnce);
        let id = expected.id;

        {
            let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
            coordinator.begin(expected, b"prompt").unwrap();
        }

        let drifted = DurableEffectSpec::new(
            id,
            "Provider.ask",
            EffectBoundary::External,
            DeliverySemantics::EffectivelyOnceWithDeduplication,
        );
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert!(matches!(
            coordinator.begin(drifted, b"prompt"),
            Err(DurableEffectRuntimeError::SpecMismatch { effect_id, .. }) if effect_id == id
        ));
        assert_eq!(store.latest_sequence(42), 1);
    }

    #[test]
    fn stale_activation_cannot_prepare_new_effect() {
        let mut store = MemoryStore::new();

        {
            let mut current = DurableEffectCoordinator::new(&mut store, 42, 2);
            current
                .begin(spec(42, "turn:1", DeliverySemantics::AtLeastOnce), b"first")
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

    #[test]
    fn stale_activation_cannot_redispatch_an_already_prepared_effect() {
        let mut store = MemoryStore::new();
        let prepared = spec(
            42,
            "turn:prepared",
            DeliverySemantics::EffectivelyOnceWithDeduplication,
        );
        {
            let mut first = DurableEffectCoordinator::new(&mut store, 42, 1);
            first.begin(prepared.clone(), b"charge").unwrap();
        }
        {
            let mut next = DurableEffectCoordinator::new(&mut store, 42, 2);
            next.begin(spec(42, "turn:advance", DeliverySemantics::AtLeastOnce), b"other")
                .unwrap();
        }
        let mut stale = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert!(matches!(
            stale.begin(prepared, b"charge"),
            Err(DurableEffectRuntimeError::Storage(ref error))
                if error.kind() == io::ErrorKind::PermissionDenied
        ));
        assert_eq!(store.latest_sequence(42), 2);
    }

    #[test]
    fn unclaimed_future_activation_cannot_redispatch_existing_intent() {
        let mut store = MemoryStore::new();
        let prepared = spec(42, "turn:prepared", DeliverySemantics::AtLeastOnce);
        {
            let mut original = DurableEffectCoordinator::new(&mut store, 42, 1);
            original.begin(prepared.clone(), b"request").unwrap();
        }

        // Epoch 2 has not made a durable claim. It cannot safely assume
        // that the epoch-1 owner has been fenced away.
        let mut unclaimed = DurableEffectCoordinator::new(&mut store, 42, 2);
        assert!(matches!(
            unclaimed.begin(prepared, b"request"),
            Err(DurableEffectRuntimeError::Storage(ref error))
                if error.kind() == io::ErrorKind::PermissionDenied
        ));
        assert_eq!(store.latest_sequence(42), 1);
    }

    #[test]
    fn completed_result_can_be_read_without_dispatch_from_old_epoch() {
        let mut store = MemoryStore::new();
        let prepared = spec(42, "turn:receipt", DeliverySemantics::AtLeastOnce);
        {
            let mut first = DurableEffectCoordinator::new(&mut store, 42, 1);
            first.begin(prepared.clone(), b"request").unwrap();
            first.complete(prepared.id, b"request", b"receipt".to_vec()).unwrap();
        }
        {
            let mut next = DurableEffectCoordinator::new(&mut store, 42, 2);
            next.begin(spec(42, "turn:advance", DeliverySemantics::AtLeastOnce), b"other")
                .unwrap();
        }
        let mut old = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert_eq!(
            old.begin(prepared, b"request").unwrap(),
            DurableEffectDispatchDecision::ReplayRecordedResult(b"receipt".to_vec())
        );
        assert_eq!(store.latest_sequence(42), 3);
    }
}
