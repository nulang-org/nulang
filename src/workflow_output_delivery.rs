//! Durable, replay-stable delivery of a verified workflow output to a
//! **trusted-host**, **provider-idempotent** sink.
//!
//! This does not promise unconditional exactly-once delivery. A remote service
//! may commit a request immediately before the client loses its response. The
//! sink MUST durably deduplicate the supplied idempotency key (or return its
//! previous receipt) when Prepared is recovered and dispatched again.

use crate::durable_effect::DurableEffectSpec;
use crate::durable_effect_runtime::{
    DurableEffectCoordinator, DurableEffectDispatchDecision, DurableEffectRuntimeError,
};
use crate::primitives::{DeliverySemantics, EffectBoundary};
use crate::runtime::{PersistedValue, PersistenceStore};
use crate::workflow_output::{
    read_workflow_output_receipts, WorkflowOutputError, WorkflowOutputReceipt,
};
use std::fmt;
use std::io;

#[derive(Debug)]
pub enum WorkflowOutputDeliveryError {
    InvalidDestination,
    InvalidReceipt(WorkflowOutputError),
    ReceiptMismatch,
    Recovery(DurableEffectRuntimeError),
    Provider(io::Error),
    UnexpectedDispatchDecision,
}

impl fmt::Display for WorkflowOutputDeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDestination => write!(
                f,
                "workflow output destination must be 1..2048 non-whitespace bytes"
            ),
            Self::InvalidReceipt(error) => error.fmt(f),
            Self::ReceiptMismatch => write!(
                f,
                "workflow output receipt differs from the persisted journal"
            ),
            Self::Recovery(error) => error.fmt(f),
            Self::Provider(error) => write!(f, "workflow output provider error: {error}"),
            Self::UnexpectedDispatchDecision => write!(
                f,
                "workflow output delivery requires a deduplicating provider"
            ),
        }
    }
}
impl std::error::Error for WorkflowOutputDeliveryError {}

/// Verify an immutable output receipt, atomically persist intent before calling
/// the provider, then durably persist its response. Completed results are
/// replayed without provider calls. The sink MUST treat the `key` as a durable
/// deduplication key; simply accepting or logging it is insufficient.
///
/// `actor_id`, `activation_epoch`, and `destination` are trusted host inputs.
/// The host MUST authenticate the actor's ownership/fencing epoch and authorize
/// the concrete destination before calling this function. This is NOT a VM
/// ambient effect or a general network authorization grant.
///
/// `dispatch(key, destination, text_bytes)` MUST return the same receipt on
/// duplicate `key` after a crash. Provider failures leave the journal Prepared
/// so the caller can recover and retry with the same key. Invalid receipts,
/// destination drift, or storage errors never invoke the provider.
pub fn deliver_output_with_deduplication<F>(
    store: &mut dyn PersistenceStore,
    actor_id: u64,
    activation_epoch: u64,
    receipt: &WorkflowOutputReceipt,
    destination: &str,
    dispatch: F,
) -> Result<Vec<u8>, WorkflowOutputDeliveryError>
where
    F: FnOnce(&str, &str, &[u8]) -> io::Result<Vec<u8>>,
{
    if destination.is_empty() || destination.len() > 2048 || destination.trim().is_empty() {
        return Err(WorkflowOutputDeliveryError::InvalidDestination);
    }
    // The projection scans and verifies the entire workflow journal even for
    // a single-item page, so a corrupt later record fails closed.
    let matched =
        read_workflow_output_receipts(store, actor_id, receipt.sequence.saturating_sub(1), 1)
            .map_err(WorkflowOutputDeliveryError::InvalidReceipt)?;
    if matched.len() != 1 || matched.first() != Some(receipt) {
        return Err(WorkflowOutputDeliveryError::ReceiptMismatch);
    }
    let PersistedValue::String(ref text) = receipt.value else {
        return Err(WorkflowOutputDeliveryError::ReceiptMismatch);
    };
    // Serialized tuple (rather than concatenated strings) binds destination,
    // journal sequence and payload without length-boundary ambiguity. Include
    // an explicit envelope/version discriminator for future migrations.
    let request = serde_json::to_vec(&(
        "nulang.workflow-output-delivery.v1",
        actor_id,
        receipt.sequence,
        receipt.replay_id.activation.command_sequence,
        receipt.replay_id.ordinal,
        destination,
        text,
    ))
    .map_err(|error| {
        WorkflowOutputDeliveryError::Provider(io::Error::new(io::ErrorKind::InvalidData, error))
    })?;

    let spec = DurableEffectSpec::new(
        receipt.durable_effect_id(),
        "WorkflowOutput.deliver",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    let mut coordinator = DurableEffectCoordinator::new(store, actor_id, activation_epoch);
    match coordinator
        .begin(spec, &request)
        .map_err(WorkflowOutputDeliveryError::Recovery)?
    {
        DurableEffectDispatchDecision::ReplayRecordedResult(previous) => Ok(previous),
        DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id } => {
            let response = dispatch(
                &operation_id.idempotency_key(),
                destination,
                text.as_bytes(),
            )
            .map_err(WorkflowOutputDeliveryError::Provider)?;
            coordinator
                .complete(operation_id, &request, response)
                .map_err(WorkflowOutputDeliveryError::Recovery)
        }
        _ => Err(WorkflowOutputDeliveryError::UnexpectedDispatchDecision),
    }
}
