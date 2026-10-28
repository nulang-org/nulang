//! Server-owned, transport-neutral UI session coordinator.
//!
//! Session access is provided by the host's trusted verifier on *every*
//! operation. Client-supplied IDs, revision numbers, or input sequences never
//! establish authority. This module does not register any HTTP endpoint.
use crate::web::ui_template::{TemplateError, UiTemplate, UiTemplateManifest};
use nulang_ui_protocol::{
    DocumentId, InputAcknowledgement, NodeId, ProtocolError, Revision, RuntimeToHostMessage,
    UiDocument, WireValue,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

const MAX_INPUT_KEYS: usize = 4096;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiSessionScope {
    pub tenant: String,
    pub subject: String,
    pub session: String,
    pub document_id: DocumentId,
    /// The compiler-declared view, set by UiLiveSession::open before authorization.
    pub view: String,
}

impl UiSessionScope {
    pub fn new(
        tenant: impl Into<String>,
        subject: impl Into<String>,
        session: impl Into<String>,
        document_id: impl Into<DocumentId>,
    ) -> Self {
        Self {
            tenant: tenant.into(),
            subject: subject.into(),
            session: session.into(),
            document_id: document_id.into(),
            view: String::new(),
        }
    }

    fn valid(&self) -> bool {
        [
            &self.tenant,
            &self.subject,
            &self.session,
            &self.document_id.0,
            &self.view,
        ]
        .into_iter()
        .all(|v| {
            !v.trim().is_empty()
                && v.trim() == v
                && v.len() <= 256
                && !v.chars().any(char::is_control)
        })
    }
}

/// A client-originated input can be acknowledged only after a trusted server
/// handler validates and incorporates the edit. Never trust this structure by
/// itself as proof of acceptance or authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiInputCommit {
    pub node_id: NodeId,
    pub property: String,
    pub sequence: Revision,
    pub value: WireValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiSessionError {
    Unauthorized,
    Revoked,
    InvalidScope,
    InvalidArtifact,
    UnsupportedManifest,
    UnknownView,
    Template(TemplateError),
    Protocol(ProtocolError),
    InvalidInput(String),
    Replay,
    ServerRejected,
    RevisionExhausted,
}

impl std::fmt::Display for UiSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for UiSessionError {}

/// Scope-bound session for one compiled view. Auth callbacks must bind the
/// currently verified HTTP/SSE principal to the scope, including its tenant,
/// subject, specific document/session and view permission. Consult revocation
/// on every access. A static allow-all callback is only suitable for tests.
pub struct UiLiveSession {
    scope: UiSessionScope,
    template: UiTemplate,
    document: UiDocument,
    accepted_sequences: BTreeMap<(NodeId, String), Revision>,
    revoked: bool,
}

impl UiLiveSession {
    /// Load the compiler-owned *private* manifest from the server's trusted
    /// build directory, after the current request has passed authorization.
    /// The file path must come from deployment configuration, never from
    /// a client-supplied filename or public `dist/` URL.
    pub fn open_compiled_manifest(
        path: &Path,
        view: &str,
        mut scope: UiSessionScope,
        bindings: &BTreeMap<String, WireValue>,
        authorize: &impl Fn(&UiSessionScope) -> bool,
    ) -> Result<Self, UiSessionError> {
        scope.view = view.to_owned();
        if !scope.valid() {
            return Err(UiSessionError::InvalidScope);
        }
        if !authorize(&scope) {
            return Err(UiSessionError::Unauthorized);
        }
        let file = std::fs::File::open(path).map_err(|_| UiSessionError::InvalidArtifact)?;
        // Use a capped streaming read, not read_to_end on an untrusted size
        // or an unchecked stale metadata length.
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| UiSessionError::InvalidArtifact)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(UiSessionError::InvalidArtifact);
        }
        let manifest: UiTemplateManifest =
            serde_json::from_slice(&bytes).map_err(|_| UiSessionError::InvalidArtifact)?;
        Self::open(&manifest, view, scope, bindings, authorize)
    }

    pub fn open(
        manifest: &UiTemplateManifest,
        view: &str,
        mut scope: UiSessionScope,
        bindings: &BTreeMap<String, WireValue>,
        authorize: &impl Fn(&UiSessionScope) -> bool,
    ) -> Result<Self, UiSessionError> {
        scope.view = view.to_owned();
        if !scope.valid() {
            return Err(UiSessionError::InvalidScope);
        }
        // Check authorization before looking up or rendering templates.
        if !authorize(&scope) {
            return Err(UiSessionError::Unauthorized);
        }
        if manifest.version != 1 {
            return Err(UiSessionError::UnsupportedManifest);
        }
        let template = manifest
            .templates
            .get(view)
            .ok_or(UiSessionError::UnknownView)?
            .clone();
        if template.function != view {
            return Err(UiSessionError::UnknownView);
        }
        let document = template
            .render(scope.document_id.as_str(), Revision(1), bindings)
            .map_err(UiSessionError::Template)?;
        document.validate().map_err(UiSessionError::Protocol)?;
        Ok(Self {
            scope,
            template,
            document,
            accepted_sequences: BTreeMap::new(),
            revoked: false,
        })
    }

    /// Non-sensitive revision metadata only. Reading full snapshots requires
    /// a current, verified access check; see `snapshot`.
    pub fn revision(&self) -> Revision {
        self.document.revision
    }

    pub fn revoke(&mut self) {
        self.revoked = true;
    }

    fn check_access(
        &self,
        authorize: &impl Fn(&UiSessionScope) -> bool,
    ) -> Result<(), UiSessionError> {
        if self.revoked {
            return Err(UiSessionError::Revoked);
        }
        if !authorize(&self.scope) {
            return Err(UiSessionError::Unauthorized);
        }
        Ok(())
    }

    pub fn snapshot(
        &self,
        authorize: &impl Fn(&UiSessionScope) -> bool,
    ) -> Result<RuntimeToHostMessage, UiSessionError> {
        self.check_access(authorize)?;
        Ok(RuntimeToHostMessage::snapshot(self.document.clone()))
    }

    /// Compute the next authoritative UI tree and emit an ordered patch.
    ///
    /// For input acknowledgements, `server_accepted` must be a trusted,
    /// server-side predicate reflecting application-committed inputs (ideally
    /// in the same DB transaction). This method validates input membership,
    /// sequence high-water marks, types and the resulting DOM value before
    /// calling it. If any check fails, no UI revision or acknowledgement is
    /// committed. This API does NOT itself provide database transactions.
    pub fn commit(
        &mut self,
        bindings: &BTreeMap<String, WireValue>,
        inputs: &[UiInputCommit],
        authorize: &impl Fn(&UiSessionScope) -> bool,
        server_accepted: &impl Fn(&[UiInputCommit]) -> bool,
    ) -> Result<RuntimeToHostMessage, UiSessionError> {
        self.check_access(authorize)?;
        let version = self
            .document
            .revision
            .0
            .checked_add(1)
            .ok_or(UiSessionError::RevisionExhausted)?;
        let next = self
            .template
            .render(self.scope.document_id.as_str(), Revision(version), bindings)
            .map_err(UiSessionError::Template)?;

        let before = self
            .document
            .nodes
            .iter()
            .map(|n| (&n.id, n))
            .collect::<BTreeMap<_, _>>();
        let after = next
            .nodes
            .iter()
            .map(|n| (&n.id, n))
            .collect::<BTreeMap<_, _>>();
        let mut seen = BTreeSet::new();
        let mut next_sequences = self.accepted_sequences.clone();
        let mut acks = Vec::with_capacity(inputs.len());

        for input in inputs {
            let key = (input.node_id.clone(), input.property.clone());
            if !seen.insert(key.clone()) {
                return Err(UiSessionError::InvalidInput(
                    "duplicate input in commit".into(),
                ));
            }
            if !matches!(input.property.as_str(), "value" | "checked") {
                return Err(UiSessionError::InvalidInput(
                    "unsupported input property".into(),
                ));
            }
            if input.sequence.0 == 0
                || next_sequences
                    .get(&key)
                    .is_some_and(|previous| input.sequence <= *previous)
            {
                return Err(UiSessionError::Replay);
            }
            let (Some(old), Some(new)) = (before.get(&input.node_id), after.get(&input.node_id))
            else {
                return Err(UiSessionError::InvalidInput("unbound input node".into()));
            };
            if old.kind != new.kind
                || !matches!(new.kind.as_str(), "input" | "textarea" | "select")
                || !matches!(
                    (&*input.property, &input.value),
                    ("value", WireValue::String(_)) | ("checked", WireValue::Bool(_))
                )
                || new.properties.get(&input.property) != Some(&input.value)
            {
                return Err(UiSessionError::InvalidInput(
                    "input not incorporated in the authoritative DOM".into(),
                ));
            }
            next_sequences.insert(key, input.sequence);
            acks.push(InputAcknowledgement {
                node_id: input.node_id.clone(),
                property: input.property.clone(),
                sequence: input.sequence,
            });
        }
        if next_sequences.len() > MAX_INPUT_KEYS {
            return Err(UiSessionError::InvalidInput(
                "input sequence budget exceeded".into(),
            ));
        }
        let mut patch = self
            .document
            .diff(&next)
            .map_err(UiSessionError::Protocol)?;
        patch.acknowledged_inputs = acks;
        patch.validate().map_err(UiSessionError::Protocol)?;

        if !inputs.is_empty() && !server_accepted(inputs) {
            return Err(UiSessionError::ServerRejected);
        }
        // Authorization may have been revoked while the application
        // processed the batch. Recheck before advancing the session.
        self.check_access(authorize)?;
        // The rendered state, patch and server-owned ack high-water marks move
        // forward together, after all checks and acceptance have succeeded.
        self.document = next;
        self.accepted_sequences = next_sequences;
        Ok(RuntimeToHostMessage::patch(patch))
    }
}
