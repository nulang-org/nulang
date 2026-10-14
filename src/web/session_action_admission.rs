//! Admission for renderer-neutral server actions bound to authenticated sessions.
//!
//! A future HTTP host must supply a verified identity and server-owned document.
//! The registry is an in-memory non-executing primitive, not a persistent effect
//! receipt store or an authentication provider.

use crate::bytecode::CodeModule;
use crate::web::action_dispatch::{
    bind_document_action_payload, BoundUiAction, UiActionDispatchError,
};
use crate::web::contracts::HandlerParamContract;
use nulang_ui_protocol::{
    ActionId, ActionPlacement, ActionRequest, DocumentId, HostToRuntimeMessage, IdempotencyKey,
    Revision, UiDocument, WireValue,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

/// Opaque to request payloads. Only a host that has already authenticated the
/// request may construct this value; string validation is NOT authentication.
/// A future integration must bind these IDs to its trusted session middleware.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedUiIdentity {
    session_id: String,
    principal_id: String,
}

impl VerifiedUiIdentity {
    pub(crate) fn from_verified_host(session_id: &str, principal_id: &str) -> Option<Self> {
        if session_id.trim().is_empty() || principal_id.trim().is_empty() {
            return None;
        }
        Some(Self {
            session_id: session_id.to_owned(),
            principal_id: principal_id.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    UnknownSession,
    UnknownDocument,
    SessionOwnerMismatch,
    InvalidDocument(String),
    NonIncreasingRevision,
    ReplayConflict,
    Binding(UiActionDispatchError),
    InternalStateUnavailable,
    CapacityExceeded,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AdmissionDecision {
    /// Authorization, parameter binding, and reservation passed. The caller
    /// still must enforce effect authority before scheduling execution.
    Accepted(BoundUiAction),
    /// Same session, idempotency key, and semantic invocation already admitted.
    /// Never execute this duplicate; the existing receipt must be consulted.
    Duplicate,
}

#[derive(Clone, PartialEq, Eq)]
struct InvocationSignature {
    document_id: DocumentId,
    revision: Revision,
    action_id: ActionId,
    placement: ActionPlacement,
    payload: WireValue,
}

impl From<&ActionRequest> for InvocationSignature {
    fn from(request: &ActionRequest) -> Self {
        Self {
            document_id: request.document_id.clone(),
            revision: request.revision,
            action_id: request.action_id.clone(),
            placement: request.placement,
            payload: request.payload.clone(),
        }
    }
}

struct SessionState {
    owner_id: String,
    documents: BTreeMap<DocumentId, UiDocument>,
    admitted: BTreeMap<IdempotencyKey, InvocationSignature>,
}

/// In-memory admission fence only. Reservations are not durable receipts and
/// do not survive a process crash; this type MUST NOT back externally effectful
/// actions until persistence/fencing and session lifecycle integration exist.
///
/// A mutex keeps check + reserve atomic across concurrently arriving requests.
/// Conservative fail-closed admission limits. Capacity exhaustion must never
/// evict an idempotency key: that could re-authorize a previously admitted
/// external mutation. These defaults limit process growth, not per-document
/// payload size; a production host still needs bounded transport input.
#[derive(Debug, Clone, Copy)]
struct AdmissionLimits {
    max_sessions: usize,
    max_documents_per_session: usize,
    max_admissions_per_session: usize,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            max_sessions: 1024,
            max_documents_per_session: 16,
            max_admissions_per_session: 2048,
        }
    }
}

pub(crate) struct SessionActionAdmission {
    sessions: Mutex<HashMap<String, SessionState>>,
    limits: AdmissionLimits,
}

impl Default for SessionActionAdmission {
    fn default() -> Self {
        Self::with_limits(AdmissionLimits::default())
    }
}

impl SessionActionAdmission {
    fn with_limits(limits: AdmissionLimits) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            limits,
        }
    }
}

impl SessionActionAdmission {
    /// Install a document obtained from the server renderer, not the client.
    /// A document's revision may only advance for an existing session.
    pub(crate) fn register_document(
        &self,
        identity: &VerifiedUiIdentity,
        document: UiDocument,
    ) -> Result<(), AdmissionError> {
        document
            .validate()
            .map_err(|error| AdmissionError::InvalidDocument(error.to_string()))?;

        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| AdmissionError::InternalStateUnavailable)?;

        if let Some(session) = sessions.get_mut(&identity.session_id) {
            if session.owner_id != identity.principal_id {
                return Err(AdmissionError::SessionOwnerMismatch);
            }
            if let Some(current) = session.documents.get(&document.document_id) {
                if document.revision <= current.revision {
                    return Err(AdmissionError::NonIncreasingRevision);
                }
            }
            if !session.documents.contains_key(&document.document_id)
                && session.documents.len() >= self.limits.max_documents_per_session
            {
                return Err(AdmissionError::CapacityExceeded);
            }
            session
                .documents
                .insert(document.document_id.clone(), document);
        } else {
            if sessions.len() >= self.limits.max_sessions
                || self.limits.max_documents_per_session == 0
            {
                return Err(AdmissionError::CapacityExceeded);
            }
            let mut documents = BTreeMap::new();
            documents.insert(document.document_id.clone(), document);
            sessions.insert(
                identity.session_id.clone(),
                SessionState {
                    owner_id: identity.principal_id.clone(),
                    documents,
                    admitted: BTreeMap::new(),
                },
            );
        }
        Ok(())
    }

    /// Non-executing reservation. Duplicate keys are scoped to the authenticated
    /// session and compared by semantic invocation, not by trace/correlation ID.
    ///
    /// After returning Accepted, the caller owns execution and must atomically
    /// commit an external-effect intent/receipt in durable storage if retry
    /// safety across restarts is required.
    pub(crate) fn reserve(
        &self,
        identity: &VerifiedUiIdentity,
        module: &CodeModule,
        message: &HostToRuntimeMessage,
        compiler_params: &[HandlerParamContract],
    ) -> Result<AdmissionDecision, AdmissionError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| AdmissionError::InternalStateUnavailable)?;
        let session = sessions
            .get_mut(&identity.session_id)
            .ok_or(AdmissionError::UnknownSession)?;
        if session.owner_id != identity.principal_id {
            return Err(AdmissionError::SessionOwnerMismatch);
        }

        message.validate().map_err(|error| {
            AdmissionError::Binding(UiActionDispatchError::InvalidEnvelope(error.to_string()))
        })?;
        let HostToRuntimeMessage::InvokeAction { request, .. } = message;
        // Exact semantic replay is checked before revision validation so a
        // retry cannot re-execute merely because the document has advanced.
        // Envelope structure is re-validated in bind_document_action_payload.
        if let Some(earlier) = session.admitted.get(&request.idempotency_key) {
            return if *earlier == InvocationSignature::from(request) {
                Ok(AdmissionDecision::Duplicate)
            } else {
                Err(AdmissionError::ReplayConflict)
            };
        }

        let document = session
            .documents
            .get(&request.document_id)
            .ok_or(AdmissionError::UnknownDocument)?;
        let bound = bind_document_action_payload(module, document, message, compiler_params)
            .map_err(AdmissionError::Binding)?;
        if session.admitted.len() >= self.limits.max_admissions_per_session {
            return Err(AdmissionError::CapacityExceeded);
        }
        session.admitted.insert(
            request.idempotency_key.clone(),
            InvocationSignature::from(request),
        );
        Ok(AdmissionDecision::Accepted(bound))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CodeModule, DebugFunctionInfo};
    use crate::web::contracts::HandlerParamContract;
    use nulang_ui_protocol::{
        ActionBinding, ActionPlacement, ActionRequest, HostToRuntimeMessage, Revision, UiNode,
        WireValue,
    };

    fn identity(session: &str, principal: &str) -> VerifiedUiIdentity {
        VerifiedUiIdentity::from_verified_host(session, principal)
            .expect("nonempty host-verified identity")
    }

    fn document(revision: u64) -> UiDocument {
        let mut root = UiNode::new("root", "interaction_root");
        root.actions.push(ActionBinding {
            event: "activate".to_owned(),
            action_id: "save".into(),
            placement: ActionPlacement::Server,
        });
        UiDocument::new("tasks", Revision(revision), "root", vec![root])
    }

    fn module() -> CodeModule {
        let mut module = CodeModule::new("actions");
        module.function_table.push(0);
        module.debug_functions.push(DebugFunctionInfo {
            name: "save".to_owned(),
            code_offset: 0,
            code_len: 0,
            params: vec![0],
            locals: Vec::new(),
        });
        module
    }

    fn params() -> Vec<HandlerParamContract> {
        vec![HandlerParamContract {
            name: "title".to_owned(),
            ty: Some("String".to_owned()),
            capability: None,
            request: None,
        }]
    }

    fn request(revision: u64, key: &str, title: &str) -> HostToRuntimeMessage {
        HostToRuntimeMessage::invoke_action(ActionRequest {
            document_id: "tasks".into(),
            revision: Revision(revision),
            action_id: "save".into(),
            placement: ActionPlacement::Server,
            correlation_id: "trace-1".into(),
            idempotency_key: key.into(),
            payload: WireValue::Object(
                [("title".to_owned(), WireValue::from(title))]
                    .into_iter()
                    .collect(),
            ),
        })
    }

    #[test]
    fn admits_one_action_then_deduplicates_same_envelope() {
        let registry = SessionActionAdmission::default();
        let ctx = identity("session-1", "owner-1");
        registry.register_document(&ctx, document(1)).unwrap();
        let message = request(1, "mutation-1", "Hello");

        assert!(matches!(
            registry.reserve(&ctx, &module(), &message, &params()),
            Ok(AdmissionDecision::Accepted(_))
        ));
        assert_eq!(
            registry.reserve(&ctx, &module(), &message, &params()),
            Ok(AdmissionDecision::Duplicate)
        );
        assert_eq!(
            registry.reserve(
                &ctx,
                &module(),
                &request(1, "mutation-1", "Changed"),
                &params()
            ),
            Err(AdmissionError::ReplayConflict)
        );
    }

    #[test]
    fn rejects_foreign_principal_and_unregistered_session() {
        let registry = SessionActionAdmission::default();
        let owner = identity("session-1", "owner-1");
        registry.register_document(&owner, document(1)).unwrap();

        let foreign = identity("session-1", "owner-2");
        assert_eq!(
            registry.reserve(
                &foreign,
                &module(),
                &request(1, "key-1", "Hello"),
                &params()
            ),
            Err(AdmissionError::SessionOwnerMismatch)
        );
        assert_eq!(
            registry.register_document(&foreign, document(2)),
            Err(AdmissionError::SessionOwnerMismatch)
        );
        assert_eq!(
            registry.reserve(
                &identity("session-2", "owner-1"),
                &module(),
                &request(1, "key-2", "Hello"),
                &params()
            ),
            Err(AdmissionError::UnknownSession)
        );
    }

    #[test]
    fn rejects_invalid_document_and_revision_regression() {
        let registry = SessionActionAdmission::default();
        let owner = identity("session-1", "owner-1");
        let mut invalid = document(1);
        invalid.root = "missing".into();
        assert!(matches!(
            registry.register_document(&owner, invalid),
            Err(AdmissionError::InvalidDocument(_))
        ));

        registry.register_document(&owner, document(3)).unwrap();
        assert_eq!(
            registry.register_document(&owner, document(2)),
            Err(AdmissionError::NonIncreasingRevision)
        );
        assert!(matches!(
            registry.reserve(&owner, &module(), &request(2, "key-1", "Hello"), &params()),
            Err(AdmissionError::Binding(_))
        ));
    }

    #[test]
    fn invalid_binding_does_not_reserve_idempotency_key() {
        let registry = SessionActionAdmission::default();
        let owner = identity("session-1", "owner-1");
        registry.register_document(&owner, document(1)).unwrap();

        let mut bad = request(1, "same-key", "Hello");
        let HostToRuntimeMessage::InvokeAction {
            request: forged_request,
            ..
        } = &mut bad;
        forged_request.action_id = "unregistered".into();
        assert!(matches!(
            registry.reserve(&owner, &module(), &bad, &params()),
            Err(AdmissionError::Binding(_))
        ));
        assert!(matches!(
            registry.reserve(
                &owner,
                &module(),
                &request(1, "same-key", "Hello"),
                &params()
            ),
            Ok(AdmissionDecision::Accepted(_))
        ));
    }

    #[test]
    fn duplicate_is_rejected_without_reexecuting_after_document_advances() {
        let registry = SessionActionAdmission::default();
        let owner = identity("session-1", "owner-1");
        registry.register_document(&owner, document(1)).unwrap();
        let original = request(1, "key-1", "Hello");
        registry
            .reserve(&owner, &module(), &original, &params())
            .unwrap();

        registry.register_document(&owner, document(2)).unwrap();
        assert_eq!(
            registry.reserve(&owner, &module(), &original, &params()),
            Ok(AdmissionDecision::Duplicate)
        );
        assert!(matches!(
            registry.reserve(&owner, &module(), &request(1, "key-2", "Hello"), &params()),
            Err(AdmissionError::Binding(_))
        ));
    }

    #[test]
    fn retries_with_new_trace_id_are_duplicates_but_bad_protocol_is_rejected() {
        let registry = SessionActionAdmission::default();
        let owner = identity("session-1", "owner-1");
        registry.register_document(&owner, document(1)).unwrap();
        let original = request(1, "key-1", "Hello");
        registry
            .reserve(&owner, &module(), &original, &params())
            .unwrap();

        let mut retried = original.clone();
        let HostToRuntimeMessage::InvokeAction { request, .. } = &mut retried;
        request.correlation_id = "new-transport-attempt".into();
        assert_eq!(
            registry.reserve(&owner, &module(), &retried, &params()),
            Ok(AdmissionDecision::Duplicate)
        );

        let HostToRuntimeMessage::InvokeAction { protocol, .. } = &mut retried;
        *protocol = "nulang-ui-msg/invalid".into();
        assert!(matches!(
            registry.reserve(&owner, &module(), &retried, &params()),
            Err(AdmissionError::Binding(
                crate::web::action_dispatch::UiActionDispatchError::InvalidEnvelope(_)
            ))
        ));
    }

    #[test]
    fn concurrent_retries_have_exactly_one_admission_in_this_process() {
        use std::sync::Arc;
        let registry = Arc::new(SessionActionAdmission::default());
        let owner = identity("session-1", "owner-1");
        registry.register_document(&owner, document(1)).unwrap();

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let owner = owner.clone();
                std::thread::spawn(move || {
                    registry.reserve(
                        &owner,
                        &module(),
                        &request(1, "shared-key", "Hello"),
                        &params(),
                    )
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let accepted = results
            .iter()
            .filter(|result| matches!(result, Ok(AdmissionDecision::Accepted(_))))
            .count();
        let duplicate = results
            .iter()
            .filter(|result| matches!(result, Ok(AdmissionDecision::Duplicate)))
            .count();
        assert_eq!(accepted, 1);
        assert_eq!(duplicate, 7);
    }

    #[test]
    fn capacity_limits_fail_closed_without_evicting_replay_entries() {
        let registry = SessionActionAdmission::with_limits(AdmissionLimits {
            max_sessions: 1,
            max_documents_per_session: 1,
            max_admissions_per_session: 1,
        });
        let owner = identity("session-1", "owner-1");
        registry.register_document(&owner, document(1)).unwrap();

        let other_session = identity("session-2", "owner-2");
        assert_eq!(
            registry.register_document(&other_session, document(1)),
            Err(AdmissionError::CapacityExceeded)
        );

        let another_document = UiDocument::new(
            "unrelated",
            Revision(1),
            "root",
            vec![UiNode::new("root", "interaction_root")],
        );
        assert_eq!(
            registry.register_document(&owner, another_document),
            Err(AdmissionError::CapacityExceeded)
        );
        // Updating an already-registered document does not consume a slot.
        registry.register_document(&owner, document(2)).unwrap();

        let admitted = request(2, "key-1", "Hello");
        assert!(matches!(
            registry.reserve(&owner, &module(), &admitted, &params()),
            Ok(AdmissionDecision::Accepted(_))
        ));
        assert_eq!(
            registry.reserve(&owner, &module(), &admitted, &params()),
            Ok(AdmissionDecision::Duplicate)
        );
        assert_eq!(
            registry.reserve(
                &owner,
                &module(),
                &request(2, "key-1", "Changed"),
                &params()
            ),
            Err(AdmissionError::ReplayConflict)
        );
        assert_eq!(
            registry.reserve(&owner, &module(), &request(2, "key-2", "Hello"), &params()),
            Err(AdmissionError::CapacityExceeded)
        );
    }

    #[test]
    fn host_verified_identity_must_have_nonempty_parts() {
        assert!(VerifiedUiIdentity::from_verified_host("", "owner").is_none());
        assert!(VerifiedUiIdentity::from_verified_host("session", " ").is_none());
    }
}
