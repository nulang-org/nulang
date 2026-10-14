//! Atomic storage boundary for starting and verifying an artifact-pinned workflow.
//!
//! This module is intentionally storage-neutral. It is not a substitute for
//! wiring an executable/artifact resolver into Runtime::recover_actor.

use crate::behavior_manifest::BehaviorManifest;
use crate::execution_compatibility::DurableExecutionPin;
use crate::runtime::persistence::{
    ActorSnapshot, DurableCommit, DurableTransition, PersistedValue, PersistenceStore,
    WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use std::io;

const EXECUTION_PIN_EVENT: &str = "__nulang.execution_pin/v1";

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
    let events = store.read_workflow_events(actor_id);
    if events.iter().any(|event| {
        matches!(event, WorkflowEvent::Custom { name, .. } if name == EXECUTION_PIN_EVENT)
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
                name: EXECUTION_PIN_EVENT.to_owned(),
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
    for event in store.read_workflow_events(actor_id) {
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
                if name == EXECUTION_PIN_EVENT =>
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
