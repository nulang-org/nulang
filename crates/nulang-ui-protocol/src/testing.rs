//! Deterministic, browser-free host for protocol-level UI interaction tests.
//!
//! This is not a live session server, authorization system, or browser DOM emulator.
//! It validates protocol messages and creates action envelopes from declared
//! server event bindings. Applications must still test their real handlers,
//! authentication, and browser behavior separately.

use crate::{
    ActionId, ActionPlacement, ActionRequest, CorrelationId, HostToRuntimeMessage, IdempotencyKey,
    NodeId, ProtocolError, Revision, RuntimeToHostMessage, UiDocument, UiNode, WireValue,
};
use std::fmt;

/// The current semantic document as observed by a test client.
#[derive(Debug, Clone)]
pub struct UiTestHarness {
    document: UiDocument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiTestError {
    Protocol(ProtocolError),
    ExpectedSnapshot,
    StaleSnapshot {
        current: Revision,
        received: Revision,
    },
    ConflictingSnapshot {
        revision: Revision,
    },
    UnknownNode(NodeId),
    UnknownAction {
        node_id: NodeId,
        event: String,
    },
    AmbiguousAction {
        node_id: NodeId,
        event: String,
    },
    ClientAction {
        action_id: ActionId,
    },
}

impl From<ProtocolError> for UiTestError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl fmt::Display for UiTestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(error) => write!(f, "{error}"),
            Self::ExpectedSnapshot => f.write_str("initial UI message must be a snapshot"),
            Self::StaleSnapshot { current, received } => write!(
                f,
                "snapshot revision {} is older than current revision {}",
                received.0, current.0
            ),
            Self::ConflictingSnapshot { revision } => write!(
                f,
                "snapshot revision {} conflicts with current document",
                revision.0
            ),
            Self::UnknownNode(id) => write!(f, "unknown UI node {id}"),
            Self::UnknownAction { node_id, event } => {
                write!(f, "UI node {node_id} has no {event:?} action")
            }
            Self::AmbiguousAction { node_id, event } => {
                write!(f, "UI node {node_id} has multiple {event:?} actions")
            }
            Self::ClientAction { action_id } => {
                write!(
                    f,
                    "action {action_id} is client-local and cannot dispatch to the server"
                )
            }
        }
    }
}

impl std::error::Error for UiTestError {}

impl UiTestHarness {
    /// Mount a validated server snapshot. Patches cannot initialize a test host.
    pub fn mount(message: RuntimeToHostMessage) -> Result<Self, UiTestError> {
        message.validate()?;
        match message {
            RuntimeToHostMessage::Snapshot { document, .. } => Ok(Self { document }),
            RuntimeToHostMessage::Patch { .. } => Err(UiTestError::ExpectedSnapshot),
        }
    }

    pub fn document(&self) -> &UiDocument {
        &self.document
    }

    pub fn node(&self, id: &str) -> Option<&UiNode> {
        self.document
            .nodes
            .iter()
            .find(|node| node.id.as_str() == id)
    }

    /// Apply a server message, leaving the previous document intact on error.
    ///
    /// Duplicate identical reconnect snapshots are harmless. Stale or
    /// conflicting snapshots are rejected rather than rolling state backward.
    pub fn receive(&mut self, message: RuntimeToHostMessage) -> Result<(), UiTestError> {
        message.validate()?;
        match message {
            RuntimeToHostMessage::Patch { patch, .. } => {
                self.document.apply_patch(&patch)?;
            }
            RuntimeToHostMessage::Snapshot { document, .. } => {
                if document.document_id != self.document.document_id {
                    return Err(ProtocolError::DocumentIdMismatch {
                        expected: self.document.document_id.clone(),
                        found: document.document_id,
                    }
                    .into());
                }
                if document.revision < self.document.revision {
                    return Err(UiTestError::StaleSnapshot {
                        current: self.document.revision,
                        received: document.revision,
                    });
                }
                if document.revision == self.document.revision && document != self.document {
                    return Err(UiTestError::ConflictingSnapshot {
                        revision: document.revision,
                    });
                }
                self.document = document;
            }
        }
        Ok(())
    }

    /// Construct an action message for one existing server-bound UI event.
    ///
    /// IDs are supplied explicitly so tests can assert stable correlation and
    /// idempotency semantics. This does not execute application handlers.
    pub fn action(
        &self,
        node_id: &str,
        event: &str,
        payload: WireValue,
        correlation_id: &str,
        idempotency_key: &str,
    ) -> Result<HostToRuntimeMessage, UiTestError> {
        let node = self
            .node(node_id)
            .ok_or_else(|| UiTestError::UnknownNode(NodeId::from(node_id)))?;
        let mut matches = node.actions.iter().filter(|binding| binding.event == event);
        let binding = matches.next().ok_or_else(|| UiTestError::UnknownAction {
            node_id: node.id.clone(),
            event: event.to_owned(),
        })?;
        if matches.next().is_some() {
            return Err(UiTestError::AmbiguousAction {
                node_id: node.id.clone(),
                event: event.to_owned(),
            });
        }
        if binding.placement != ActionPlacement::Server {
            return Err(UiTestError::ClientAction {
                action_id: binding.action_id.clone(),
            });
        }

        let message = HostToRuntimeMessage::invoke_action(ActionRequest {
            document_id: self.document.document_id.clone(),
            revision: self.document.revision,
            action_id: binding.action_id.clone(),
            placement: binding.placement,
            correlation_id: CorrelationId::from(correlation_id),
            idempotency_key: IdempotencyKey::from(idempotency_key),
            payload,
        });
        message.validate()?;
        Ok(message)
    }
}
