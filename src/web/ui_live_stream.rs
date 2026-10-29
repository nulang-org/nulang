//! Server-owned, opt-in SSE framing for authenticated UI session delivery.
//!
//! This is a transport adapter, not an HTTP route or public broadcast room.
//! The embedding server must reauthorize *each* subscription and each frame,
//! maintain admission limits and handle disconnects. No payload is exposed
//! before the owning session's current authorization callback succeeds.
use crate::web::ui_live_session::{UiLiveSession, UiSessionError, UiSessionScope};
use nulang_ui_protocol::{Revision, RuntimeToHostMessage};

const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiStreamError {
    Session(UiSessionError),
    InvalidCursor,
    FutureCursor,
    DocumentMismatch,
    RevisionMismatch,
    InvalidMessage,
    FrameTooLarge,
}

pub struct UiStreamDelivery;

impl UiStreamDelivery {
    /// Reconnect is deliberately snapshot-based, never replay-based: a room
    /// history cannot be shared across principals or session generations.
    /// Client cursors are *not* credentials; they only reject impossible
    /// future revisions and are never used for data selection/authorization.
    pub fn subscribe(
        session: &UiLiveSession,
        last_event_id: Option<&str>,
        authorize: &impl Fn(&UiSessionScope) -> bool,
    ) -> Result<String, UiStreamError> {
        let snapshot = session
            .snapshot(authorize)
            .map_err(UiStreamError::Session)?;
        let revision = revision_of(&snapshot);
        if let Some(cursor) = last_event_id {
            if cursor.is_empty()
                || cursor.len() > 20
                || !cursor.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(UiStreamError::InvalidCursor);
            }
            let cursor = cursor
                .parse::<u64>()
                .map_err(|_| UiStreamError::InvalidCursor)?;
            if cursor > revision.0 {
                return Err(UiStreamError::FutureCursor);
            }
        }
        let frame = encode_frame(&snapshot)?;
        // A long-running server may change permissions while encoding.
        // Recheck just before returning bytes to the HTTP writer.
        session
            .snapshot(authorize)
            .map_err(UiStreamError::Session)?;
        Ok(frame)
    }

    /// Accept only the latest authoritative message from this exact session.
    /// A stale, replayed or cross-document payload cannot be published.
    pub fn publish(
        session: &UiLiveSession,
        event: &RuntimeToHostMessage,
        authorize: &impl Fn(&UiSessionScope) -> bool,
    ) -> Result<String, UiStreamError> {
        let current = session
            .snapshot(authorize)
            .map_err(UiStreamError::Session)?;
        event
            .validate()
            .map_err(|_| UiStreamError::InvalidMessage)?;
        let expected_document = match current {
            RuntimeToHostMessage::Snapshot { document, .. } => document,
            _ => unreachable!("session snapshot always yields Snapshot"),
        };
        let (document_id, revision) = match event {
            RuntimeToHostMessage::Snapshot { document, .. } => {
                (&document.document_id, document.revision)
            }
            RuntimeToHostMessage::Patch { patch, .. } => (&patch.document_id, patch.revision),
        };
        if document_id != &expected_document.document_id {
            return Err(UiStreamError::DocumentMismatch);
        }
        if revision != expected_document.revision {
            return Err(UiStreamError::RevisionMismatch);
        }
        // An arbitrary, forged patch cannot be trusted as the authoritative
        // state, even when it has the correct document and revision.
        match event {
            RuntimeToHostMessage::Snapshot { document, .. } => {
                if document != &expected_document {
                    return Err(UiStreamError::InvalidMessage);
                }
            }
            RuntimeToHostMessage::Patch { patch, .. } => {
                if !session.is_latest_committed_event(event) {
                    return Err(UiStreamError::InvalidMessage);
                }
                if patch.base_revision >= revision || patch.operations.len() > 10_000 {
                    return Err(UiStreamError::InvalidMessage);
                }
            }
        }
        let frame = encode_frame(event)?;
        session
            .snapshot(authorize)
            .map_err(UiStreamError::Session)?;
        Ok(frame)
    }
}

fn revision_of(event: &RuntimeToHostMessage) -> Revision {
    match event {
        RuntimeToHostMessage::Snapshot { document, .. } => document.revision,
        RuntimeToHostMessage::Patch { patch, .. } => patch.revision,
    }
}

fn encode_frame(event: &RuntimeToHostMessage) -> Result<String, UiStreamError> {
    let json = serde_json::to_string(event).map_err(|_| UiStreamError::InvalidMessage)?;
    // JSON escapes input newlines and cannot inject a second SSE event.
    if json.len() > MAX_FRAME_BYTES {
        return Err(UiStreamError::FrameTooLarge);
    }
    Ok(format!(
        "event: nulang-ui\nid: {}\ndata: {json}\n\n",
        revision_of(event).0
    ))
}
