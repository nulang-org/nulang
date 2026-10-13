//! Admission for renderer-neutral server actions bound to authenticated sessions.
//!
//! A future HTTP host must supply a verified identity and server-owned document.
//! The registry is an in-memory non-executing primitive, not a persistent effect
//! receipt store or an authentication provider.

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
            registry.reserve(&ctx, &module(), &request(1, "mutation-1", "Changed"), &params()),
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
            registry.reserve(&foreign, &module(), &request(1, "key-1", "Hello"), &params()),
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
        assert_eq!(
            registry.reserve(&owner, &module(), &request(2, "key-1", "Hello"), &params()),
            Err(AdmissionError::Binding(_))
        );
    }

    #[test]
    fn invalid_binding_does_not_reserve_idempotency_key() {
        let registry = SessionActionAdmission::default();
        let owner = identity("session-1", "owner-1");
        registry.register_document(&owner, document(1)).unwrap();

        let mut bad = request(1, "same-key", "Hello");
        let HostToRuntimeMessage::InvokeAction { request, .. } = &mut bad;
        request.action_id = "unregistered".into();
        assert!(matches!(
            registry.reserve(&owner, &module(), &bad, &params()),
            Err(AdmissionError::Binding(_))
        ));
        assert!(matches!(
            registry.reserve(&owner, &module(), &request(1, "same-key", "Hello"), &params()),
            Ok(AdmissionDecision::Accepted(_))
        ));
    }

    #[test]
    fn duplicate_is_rejected_without_reexecuting_after_document_advances() {
        let registry = SessionActionAdmission::default();
        let owner = identity("session-1", "owner-1");
        registry.register_document(&owner, document(1)).unwrap();
        let original = request(1, "key-1", "Hello");
        registry.reserve(&owner, &module(), &original, &params()).unwrap();

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
    fn host_verified_identity_must_have_nonempty_parts() {
        assert!(VerifiedUiIdentity::from_verified_host("", "owner").is_none());
        assert!(VerifiedUiIdentity::from_verified_host("session", " ").is_none());
    }
}
