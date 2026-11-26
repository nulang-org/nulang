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
use crate::durable_effect_persistence::{DurableEffectPersistenceRecord, WorkflowEffectBinding};
use crate::primitives::{ActorRole, DeliverySemantics, EffectBoundary};
use crate::runtime::{
    Actor, ActorState, DurableCommit, DurableTransition, PersistenceStore, WorkflowActivationId,
    DURABLE_TRANSITION_VERSION,
};
use crate::types::{NuError, NuResult, Span};
use crate::vm::PerformAsyncResult;
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
    ActorMismatch {
        expected: u64,
        actual: u64,
    },
    ActorNotLive,
    ActorEpochMismatch {
        expected: u64,
        actual: u64,
    },
    GrantOperationMismatch {
        operation: String,
        grant: AuthorityGrant,
    },
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
            Self::ActorNotLive => write!(f, "terminated actor cannot dispatch durable effect"),
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

/// Context the trusted VM host must derive from the currently admitted actor
/// turn before an external provider is called. This is not inferred from VM
/// registers: those are ephemeral and include nonportable pointer values.
///
/// The host must check the artifact provenance separately and encode the
/// provider's *actual* request bytes deterministically. It must reuse the same
/// execution key and dynamic occurrence index after a crash. Authority must
/// come from the live actor's manifest, not an untrusted request.
#[derive(Clone, Copy)]
pub struct DurableVmAsyncInvocation<'a> {
    pub actor: &'a Actor,
    pub authority: &'a AuthorityGrant,
    pub execution_key: &'a str,
    pub occurrence_index: u32,
    pub request: &'a [u8],
}

/// Execution-local cursor for the opt-in workflow adapter. The command
/// sequence itself is durable; a fresh actor reconstructs occurrences by
/// replaying the same pinned deterministic command from its safe boundary.
/// The cursor is not persisted, but its slot-to-site binding and receipt are
/// committed together in the journal before the cursor can advance.
pub(crate) struct WorkflowEffectReplayCursor {
    activation: WorkflowActivationId,
    activation_epoch: u64,
    next_occurrence: u32,
    pending: Option<DurableEffectRecord>,
}

impl WorkflowEffectReplayCursor {
    fn new(activation: WorkflowActivationId, activation_epoch: u64) -> Self {
        Self {
            activation,
            activation_epoch,
            next_occurrence: 0,
            pending: None,
        }
    }
}

fn vm_effect_error(msg: impl Into<String>) -> NuError {
    NuError::VMError {
        msg: msg.into(),
        span: Span::default(),
    }
}

fn vm_text_receipt(receipt: Vec<u8>) -> NuResult<PerformAsyncResult> {
    let text = String::from_utf8(receipt)
        .map_err(|_| vm_effect_error("durable VM effect receipt is not valid UTF-8"))?;
    Ok(PerformAsyncResult::Ready(Some(text)))
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
            if existing.workflow_binding().is_some() {
                return Err(workflow_binding_error(
                    "bound workflow effect requires the workflow adapter",
                ));
            }
            self.validate_existing(&spec, request, existing.effect())?;
            let recovered = decision(existing.effect());
            // A completed receipt can be read without provider dispatch.
            // Prepared effects, however, must never authorize a stale or
            // unclaimed future activation to retry the external operation.
            if !matches!(
                recovered,
                DurableEffectDispatchDecision::ReplayRecordedResult(_)
            ) {
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
        let spec = self.authorized_compiled_spec(
            actor,
            authority,
            module,
            pc,
            execution_key,
            occurrence_index,
        )?;
        self.execute_authorized_spec(spec, request, dispatch)
    }

    fn authorized_compiled_spec(
        &self,
        actor: &Actor,
        authority: &AuthorityGrant,
        module: &CodeModule,
        pc: usize,
        execution_key: &str,
        occurrence_index: u32,
    ) -> Result<DurableEffectSpec, DurableAsyncDispatchError> {
        if actor.id != self.actor_id {
            return Err(DurableAsyncDispatchError::ActorMismatch {
                expected: self.actor_id,
                actual: actor.id,
            });
        }
        if actor.state == ActorState::Terminated {
            return Err(DurableAsyncDispatchError::ActorNotLive);
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

        Ok(spec)
    }

    fn execute_authorized_spec<F>(
        &mut self,
        spec: DurableEffectSpec,
        request: &[u8],
        dispatch: F,
    ) -> Result<Vec<u8>, DurableAsyncDispatchError>
    where
        F: FnOnce(&str, &[u8]) -> io::Result<Vec<u8>>,
    {
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

    fn load_bound_workflow_effect(
        &self,
        binding: WorkflowEffectBinding,
        spec: &DurableEffectSpec,
        request: &[u8],
    ) -> Result<Option<DurableEffectPersistenceRecord>, DurableEffectRuntimeError> {
        let existing = self
            .store
            .load_workflow_effect_occurrence(binding.activation, binding.occurrence)?;
        if let Some(record) = &existing {
            if record.workflow_binding() != Some(binding) {
                return Err(workflow_binding_error(
                    "durable workflow occurrence is bound to a different semantic site",
                ));
            }
            self.validate_existing(spec, request, record.effect())?;
        }
        Ok(existing)
    }

    fn execute_bound_workflow_spec<F>(
        &mut self,
        binding: WorkflowEffectBinding,
        spec: DurableEffectSpec,
        request: &[u8],
        dispatch: F,
    ) -> Result<Vec<u8>, DurableAsyncDispatchError>
    where
        F: FnOnce(&str, &[u8]) -> io::Result<Vec<u8>>,
    {
        self.require_claimed_activation()?;
        let existing = self.load_bound_workflow_effect(binding, &spec, request)?;
        if let Some(record) = &existing {
            if let DurableEffectRecoveryAction::ReplayRecordedResult(receipt) =
                record.effect().recovery_action()
            {
                return Ok(receipt.to_vec());
            }
        } else {
            let prepared = DurableEffectRecord::prepare(spec.clone(), request);
            self.commit_record(workflow_persistence_record(binding, prepared)?)?;
        }
        self.require_claimed_activation()?;
        let receipt = dispatch(&spec.id.idempotency_key(), request)
            .map_err(DurableAsyncDispatchError::Provider)?;

        // Re-read the independently keyed slot after provider execution:
        // never reconstruct a terminal record from an unverified old request.
        self.require_claimed_activation()?;
        let record = self
            .load_bound_workflow_effect(binding, &spec, request)?
            .ok_or(DurableEffectRuntimeError::MissingPreparedEffect { effect_id: spec.id })?;
        if let DurableEffectRecoveryAction::ReplayRecordedResult(result) =
            record.effect().recovery_action()
        {
            return Ok(result.to_vec());
        }
        self.commit_record(workflow_persistence_record(
            binding,
            record.effect().clone().complete(receipt.clone()),
        )?)?;
        Ok(receipt)
    }

    /// Opt-in bridge between a real VM `PerformAsync` callback and the
    /// existing durable intent/provider/receipt state machine.
    ///
    /// The VM callback supplies the exact loaded `CodeModule`, opcode PC
    /// and observed operation, while the trusted host supplies actor identity,
    /// authority, replay-stable turn/occurrence and canonical provider request.
    /// The operation is checked against compiler metadata before journaling.
    ///
    /// This adapter is deliberately limited to UTF-8 text responses and the
    /// exact unparameterized authority supported by the coordinator. Other
    /// result kinds need a versioned, lossless codec; silently turning invalid
    /// bytes or a denied dispatch into nil would violate replay semantics.
    /// On invalid UTF-8, Completed may already be durable but replay still
    /// fails closed without invoking the provider again.
    ///
    /// The artifact MUST have been verified and pinned by the host prior to
    /// this call. The default runtime callbacks do not enable this path.
    pub fn execute_vm_async_text_with_deduplication<F>(
        &mut self,
        module: &CodeModule,
        pc: usize,
        invoked_operation: &str,
        invocation: DurableVmAsyncInvocation<'_>,
        dispatch: F,
    ) -> NuResult<PerformAsyncResult>
    where
        F: FnOnce(&str, &[u8]) -> io::Result<Vec<u8>>,
    {
        // The observed VM opcode, compiler metadata and coordinator's
        // compiled-effect verification must all agree before any transition.
        let site = module.effect_site_at(pc).ok_or_else(|| NuError::VMError {
            msg: format!("durable VM effect missing compiler metadata at pc {pc}"),
            span: Span::default(),
        })?;
        if site.effect_operation != invoked_operation {
            return Err(NuError::VMError {
                msg: format!(
                    "durable VM effect operation mismatch at pc {pc}: expected {}, got {}",
                    site.effect_operation, invoked_operation
                ),
                span: Span::default(),
            });
        }
        if !invocation.actor.persistent {
            return Err(NuError::VMError {
                msg: "durable VM effect requires a persistent actor".into(),
                span: Span::default(),
            });
        }
        let receipt = self
            .execute_compiled_async_with_deduplication(
                invocation.actor,
                invocation.authority,
                module,
                pc,
                invocation.execution_key,
                invocation.occurrence_index,
                invocation.request,
                dispatch,
            )
            .map_err(|error| NuError::VMError {
                msg: format!("durable VM effect rejected: {error}"),
                span: Span::default(),
            })?;
        vm_text_receipt(receipt)
    }

    /// Experimental workflow-only adapter; the host no longer supplies an
    /// arbitrary execution key or occurrence. They come from the live actor's
    /// current accepted command and its actor-owned cursor.
    ///
    /// The trusted host MUST have admitted (or recovered) the open command
    /// atomically, verified and pinned its artifact, and replay from the safe
    /// pre-command boundary on a fresh actor. A tail check is only a necessary
    /// fence, not proof that a command is open or was atomically admitted.
    /// Do not use this with the legacy compiled-workflow scheduler.
    ///
    /// Effects are numbered in the deterministic order in which this adapter
    /// is called. The cursor survives in-process suspension; on a provider
    /// error or invalid terminal text it remains on that occurrence. Live
    /// retries cannot switch sites or requests. A fresh cursor reconstructs
    /// occurrences through deterministic replay, not by counting journal rows.
    /// The durable slot binding rejects cross-site replay drift after restart.
    /// Detecting omitted trailing effects, actor-turn atomicity and persistent
    /// continuations still require additional integration.
    pub fn execute_workflow_vm_async_text_with_deduplication<F>(
        &mut self,
        module: &CodeModule,
        pc: usize,
        invoked_operation: &str,
        actor: &mut Actor,
        authority: &AuthorityGrant,
        request: &[u8],
        dispatch: F,
    ) -> NuResult<PerformAsyncResult>
    where
        F: FnOnce(&str, &[u8]) -> io::Result<Vec<u8>>,
    {
        if !actor.persistent || !matches!(actor.role(), Ok(ActorRole::Workflow)) {
            return Err(vm_effect_error(
                "durable workflow effect requires a persistent workflow",
            ));
        }
        let activation = actor
            .current_workflow_activation
            .filter(|activation| {
                activation.actor_id == actor.id && activation.command_sequence != 0
            })
            .ok_or_else(|| {
                vm_effect_error("durable workflow effect requires the current accepted command")
            })?;

        // Treat a newer command as a new execution, but never allow a live
        // cursor to be rewound to an older command or moved to another owner.
        let occurrence = match actor.workflow_effect_cursor.as_deref() {
            Some(cursor)
                if cursor.activation_epoch != actor.activation_epoch
                    || cursor.activation.command_sequence > activation.command_sequence
                    || cursor.activation.actor_id != activation.actor_id
                    || (cursor.activation != activation && cursor.pending.is_some()) =>
            {
                return Err(vm_effect_error(
                    "durable workflow effect cursor cannot be rewound, transferred, or abandoned while pending",
                ))
            }
            Some(cursor) if cursor.activation == activation => cursor.next_occurrence,
            _ => 0,
        };
        let next = occurrence
            .checked_add(1)
            .ok_or_else(|| vm_effect_error("durable workflow effect occurrence exhausted"))?;
        let site = module
            .effect_site_at(pc)
            .ok_or_else(|| vm_effect_error("durable workflow effect missing compiler metadata"))?;
        if site.effect_operation != invoked_operation {
            return Err(vm_effect_error(
                "durable workflow effect operation mismatch",
            ));
        }
        // Actor ID is already part of the canonical effect ID. Epoch is an
        // ownership fence, not an invocation identity component.
        let binding = WorkflowEffectBinding {
            activation,
            occurrence,
            site_id: site.id,
        };
        let execution_key = binding.execution_key();
        let spec = self
            .authorized_compiled_spec(actor, authority, module, pc, &execution_key, occurrence)
            .map_err(|error| vm_effect_error(format!("durable VM effect rejected: {error}")))?;

        self.store
            .load_durable_tail_position(actor.id)
            .map_err(|error| {
                vm_effect_error(format!("durable workflow tail read failed: {error}"))
            })?
            .filter(|tail| {
                tail.sequence >= activation.command_sequence
                    && tail.activation_epoch == actor.activation_epoch
            })
            .ok_or_else(|| {
                vm_effect_error("durable workflow effect requires a current atomic tail")
            })?;
        // Keep this check separate from the coordinator's commit fence:
        // even completed replay requires an admitted live workflow owner.
        let epoch = actor.activation_epoch;

        let cursor = actor
            .workflow_effect_cursor
            .get_or_insert_with(|| Box::new(WorkflowEffectReplayCursor::new(activation, epoch)));
        if cursor.activation != activation {
            **cursor = WorkflowEffectReplayCursor::new(activation, epoch);
        }
        if let Some(pending) = &cursor.pending {
            if pending.spec() != &spec || pending.validate_request(request).is_err() {
                return Err(vm_effect_error(
                    "durable workflow effect pending occurrence changed",
                ));
            }
        } else {
            cursor.pending = Some(DurableEffectRecord::prepare(spec.clone(), request));
        }

        let receipt = self
            .execute_bound_workflow_spec(binding, spec, request, dispatch)
            .map_err(|error| vm_effect_error(format!("durable VM effect rejected: {error}")))?;
        let result = vm_text_receipt(receipt)?;
        // Only a valid terminal result advances the cursor, including replay.
        cursor.pending = None;
        cursor.next_occurrence = next;
        Ok(result)
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

        if existing.workflow_binding().is_some() {
            return Err(workflow_binding_error(
                "bound workflow effect requires the workflow adapter",
            ));
        }
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

fn workflow_binding_error(message: &str) -> DurableEffectRuntimeError {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

fn workflow_persistence_record(
    binding: WorkflowEffectBinding,
    effect: DurableEffectRecord,
) -> Result<DurableEffectPersistenceRecord, DurableEffectRuntimeError> {
    DurableEffectPersistenceRecord::from_workflow_occurrence(binding, effect)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error).into())
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
            next.begin(
                spec(42, "turn:advance", DeliverySemantics::AtLeastOnce),
                b"other",
            )
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
            first
                .complete(prepared.id, b"request", b"receipt".to_vec())
                .unwrap();
        }
        {
            let mut next = DurableEffectCoordinator::new(&mut store, 42, 2);
            next.begin(
                spec(42, "turn:advance", DeliverySemantics::AtLeastOnce),
                b"other",
            )
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
