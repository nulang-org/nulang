//! Atomic storage boundary for starting and verifying an artifact-pinned workflow.
//!
//! This module is intentionally storage-neutral. It is not a substitute for
//! wiring an executable/artifact resolver into Runtime::recover_actor.

use crate::behavior_manifest::BehaviorManifest;
use crate::execution_compatibility::DurableExecutionPin;
use crate::runtime::persistence::{
    ActorSnapshot, DurableCommit, DurableTransition, PersistedValue, PersistenceStore,
    WorkflowEvent, PINNED_WORKFLOW_START_EVENT, DURABLE_TRANSITION_VERSION,
};
use std::io;

/// Immutable, content-addressed bundle obtained from a trusted artifact store.
#[derive(Debug, Clone)]
pub struct PinnedArtifactBundle {
    pub manifest: BehaviorManifest,
    pub executable: Vec<u8>,
}

/// Host-provided immutable artifact lookup. Implementations must resolve by
/// content digest, never by a mutable tag such as "latest".
pub trait ImmutablePinnedArtifactResolver {
    fn resolve(&self, executable_digest: &str) -> io::Result<Option<PinnedArtifactBundle>>;
}

/// Guard both recognized pin records and the atomic-start shape when a
/// damaged pin record was not decoded by a legacy reader. The latter is
/// deliberately conservative; pinned callers must use strict preflight.
pub(crate) fn requires_pinned_recovery(store: &dyn PersistenceStore, actor_id: u64) -> bool {
    let events = match store.read_workflow_events_checked(actor_id) {
        Ok(events) => events,
        // Older backends without atomic pinning retain their legacy recovery
        // behavior. Atomic pin-enabled stores MUST provide strict reads.
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            store.read_workflow_events(actor_id)
        }
        Err(error) => {
            tracing::warn!(actor_id, %error, "refusing legacy recovery after unreadable workflow history");
            return true;
        }
    };
    if events.iter().any(|event| {
        matches!(event, WorkflowEvent::Custom { name, .. } if name == PINNED_WORKFLOW_START_EVENT)
    }) {
        return true;
    }
    // Atomic pinned startup has a safe snapshot at sequence 1 and a start
    // event in the same transaction. Legacy startup writes the snapshot in
    // a later independent commit. Never interpret a missing pin as an
    // invitation to use an unverified executable.
    let initial_atomic_tail = matches!(
        store.load_durable_tail_position(actor_id),
        Ok(Some(tail)) if tail.sequence >= 1
    );
    let initial_snapshot = store
        .load_snapshot(actor_id)
        .map(|snapshot| snapshot.sequence == 1)
        .unwrap_or(false);
    initial_atomic_tail
        && initial_snapshot
        && events.iter().any(|event| {
            matches!(event, WorkflowEvent::WorkflowStarted { sequence: 1, .. })
        })
}


fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

/// Decode the actual NBC bytes that passed pinned identity verification and
/// resolve exactly the actor schema recorded in the durable snapshot.
///
/// The caller MUST first run `verify_pinned_workflow_resume` on this exact
/// artifact bundle. An independently registered/unverified module is never
/// substituted for the decoded module returned by this function.
pub(crate) fn decode_verified_pinned_workflow_module(
    bundle: &PinnedArtifactBundle,
    schema_name: &str,
) -> io::Result<(crate::bytecode::CodeModule, Vec<usize>, Vec<Option<usize>>)> {
    use crate::primitives::ActorRole;

    if schema_name.trim().is_empty() {
        return Err(invalid_data("pinned bytecode workflow has an empty schema name"));
    }
    let declared_schema = bundle
        .manifest
        .actors
        .iter()
        .find(|actor| actor.name == schema_name && actor.persistence.is_durable())
        .ok_or_else(|| {
            invalid_data(
                "pinned behavior manifest does not declare the snapshot's durable actor schema",
            )
        })?;

    let module = crate::bytecode::CodeModule::from_nbc(&bundle.executable)
        .map_err(|error| invalid_data(format!("invalid pinned NBC artifact: {error}")))?
        .module;
    let meta = super::schema_identity::resolve_snapshot_actor_meta(
        &module,
        Some(schema_name),
    )
    .map_err(invalid_data)?;
    if meta.version != declared_schema.schema_version {
        return Err(invalid_data(
            "pinned NBC state schema version does not match its behavior manifest",
        ));
    }
    let role = meta.role().map_err(invalid_data)?;
    if role != ActorRole::Workflow || !meta.persistent {
        return Err(invalid_data(
            "the pinned actor schema must be a persistent bytecode workflow",
        ));
    }

    if meta.behavior_indices.is_empty() {
        return Err(invalid_data("pinned workflow has no compiled behavior entries"));
    }
    let instruction_count = module.instructions.len();
    let mut offsets = Vec::with_capacity(meta.behavior_indices.len());
    let mut compensation_offsets = Vec::with_capacity(meta.behavior_indices.len());
    let mut seen = std::collections::BTreeSet::new();
    for &index in &meta.behavior_indices {
        if !seen.insert(index) {
            return Err(invalid_data("duplicate pinned workflow behavior index"));
        }
        let behavior = module.behaviors.get(index).ok_or_else(|| {
            invalid_data("pinned workflow behavior index is outside the NBC behavior table")
        })?;
        if behavior.code_offset >= instruction_count
            || behavior
                .compensate_offset
                .is_some_and(|offset| offset >= instruction_count)
        {
            return Err(invalid_data(
                "pinned workflow contains an out-of-bounds bytecode offset",
            ));
        }
        offsets.push(behavior.code_offset);
        compensation_offsets.push(behavior.compensate_offset);
    }
    Ok((module, offsets, compensation_offsets))
}

/// Commit the first workflow checkpoint, WorkflowStarted event and immutable
/// executable pin together. No actor should become observable before this
/// function commits successfully.
///
/// Only a fresh actor history can be initialized with this method. Atomic
/// sequence/epoch fencing in each backend rejects a conflicting concurrent
/// initializer and prevents replacing the pin after a successful first commit.
pub fn commit_pinned_workflow_start(
    store: &mut dyn PersistenceStore,
    actor_id: u64,
    execution_id: &str,
    workflow_name: &str,
    manifest: &BehaviorManifest,
    executable_bytes: &[u8],
    snapshot: ActorSnapshot,
    initial_state: Vec<PersistedValue>,
) -> io::Result<DurableCommit> {
    if actor_id == 0 || workflow_name.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pinned workflow requires a nonzero actor id and workflow name",
        ));
    }
    if snapshot.actor_id != actor_id
        || snapshot.sequence != 1
        || snapshot.activation_epoch == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pinned workflow requires the exact initial actor snapshot at sequence 1",
        ));
    }
    // This API creates a new logical execution, never an idempotent replay of
    // an existing start. Reject existing history before issuing the atomic
    // commit. A cross-process identical-payload race still needs a backend
    // create-only primitive (the generic transition API deduplicates retries).
    if store.latest_sequence(actor_id) != 0
        || store.load_snapshot(actor_id).is_some()
        || store.load_durable_tail_position(actor_id)?.is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "durable actor history already exists; use pinned recovery",
        ));
    }
    let pin = DurableExecutionPin::create(execution_id, manifest, executable_bytes)
        .map_err(invalid_data)?;
    let serialized_pin = String::from_utf8(pin.to_json().map_err(invalid_data)?)
        .map_err(invalid_data)?;

    store.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: snapshot.activation_epoch,
        sequence: 1,
        expected_previous_sequence: 0,
        command: None,
        snapshot: Some(snapshot),
        workflow_events: vec![
            WorkflowEvent::WorkflowStarted {
                sequence: 1,
                name: workflow_name.to_owned(),
                state: initial_state,
            },
            WorkflowEvent::Custom {
                sequence: 1,
                replay_id: None,
                name: PINNED_WORKFLOW_START_EVENT.to_owned(),
                args: vec![PersistedValue::String(serialized_pin)],
            },
        ],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    })
}

/// Check durable storage *before* allowing an actor to be hydrated or resumed.
///
/// This method requires a committed atomic tail and a unique pin within the
/// initial transaction. It rejects legacy/unpinned histories, tampered pins,
/// conflicting identities, or changed executables. It does not itself activate
/// or schedule actor code.
pub fn verify_pinned_workflow_resume(
    store: &dyn PersistenceStore,
    actor_id: u64,
    execution_id: &str,
    manifest: &BehaviorManifest,
    executable_bytes: &[u8],
) -> io::Result<()> {
    read_pinned_workflow_pin(store, actor_id)?
        .validate_resume(execution_id, manifest, executable_bytes)
        .map_err(invalid_data)
}

/// Parse and validate the immutable pin before choosing a recovery artifact.
/// No mutable alias or latest-deployment fallback is permitted.
pub fn read_pinned_workflow_pin(
    store: &dyn PersistenceStore,
    actor_id: u64,
) -> io::Result<DurableExecutionPin> {
    let tail = store
        .load_durable_tail_position(actor_id)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no atomic workflow history"))?;
    if tail.sequence == 0 || tail.activation_epoch == 0 {
        return Err(invalid_data("invalid atomic workflow history tail"));
    }

    let snapshot = store
        .load_snapshot(actor_id)
        .ok_or_else(|| invalid_data("committed workflow snapshot is missing"))?;
    if snapshot.actor_id != actor_id
        || snapshot.sequence == 0
        || snapshot.sequence > tail.sequence
        || snapshot.activation_epoch != tail.activation_epoch
    {
        return Err(invalid_data("workflow snapshot conflicts with atomic history tail"));
    }

    let mut start_count = 0usize;
    let mut pin_count = 0usize;
    let mut serialized_pin = None;
    for event in store.read_workflow_events_checked(actor_id)? {
        match event {
            WorkflowEvent::WorkflowStarted { sequence, .. } => {
                if sequence != 1 {
                    return Err(invalid_data(
                        "pinned workflow has a WorkflowStarted event after its initial transaction",
                    ));
                }
                start_count += 1;
            }
            WorkflowEvent::Custom { sequence, name, args, replay_id }
                if name == PINNED_WORKFLOW_START_EVENT =>
            {
                pin_count += 1;
                if sequence != 1 || replay_id.is_some() || args.len() != 1 {
                    return Err(invalid_data("workflow execution pin is not an initial atomic record"));
                }
                match args.into_iter().next() {
                    Some(PersistedValue::String(pin)) => serialized_pin = Some(pin),
                    _ => return Err(invalid_data("workflow execution pin has invalid payload")),
                }
            }
            _ => {}
        }
    }
    if start_count != 1 || pin_count != 1 {
        return Err(invalid_data(
            "atomic workflow history must contain one start and one execution pin",
        ));
    }
    let pin = DurableExecutionPin::from_json(
        serialized_pin
            .as_deref()
            .ok_or_else(|| invalid_data("missing workflow execution pin"))?
            .as_bytes(),
    )
    .map_err(invalid_data)?;
    Ok(pin)
}
