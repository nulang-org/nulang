//! Deterministic, bounded views over persisted NLAP conversation history.
//!
//! This is a *content-byte* budget, not a tokenizer or a model-specific prompt
//! budget. Callers must independently reserve space for roles, tool schemas,
//! system instructions, and model framing. This projection never edits the
//! durable source messages and never promotes their roles/trust levels.

use crate::ConversationMessage;
use std::error::Error;
use std::fmt;

/// Borrowed projection of a contiguous suffix of durable conversation events.
///
/// The original message indices remain available for provenance and replay.
/// Construct model-specific contexts from this view, not by modifying history.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextProjection<'a> {
    pub messages: &'a [ConversationMessage],
    pub first_message_index: usize,
    pub omitted_count: usize,
    pub content_bytes: usize,
}

/// A projection cannot silently drop or truncate the current conversation turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextProjectionError {
    CurrentMessageExceedsBudget {
        content_bytes: usize,
        budget_bytes: usize,
    },
}

impl fmt::Display for ContextProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentMessageExceedsBudget {
                content_bytes,
                budget_bytes,
            } => write!(
                f,
                "current message is {content_bytes} bytes, exceeding context content budget {budget_bytes}"
            ),
        }
    }
}

impl Error for ContextProjectionError {}

/// Select the newest contiguous sequence of complete messages that fits.
///
/// The budget counts UTF-8 content bytes only. We preserve chronological
/// ordering, message roles, timestamps, and provenance indices verbatim. No
/// message is partially included. If the latest message alone exceeds the
/// budget, return an error rather than silently changing the user's request.
///
/// This helper does not assign system authority to any message, compress
/// content, or guarantee that a model tokenizer fits its actual token window.
pub fn project_recent_messages(
    messages: &[ConversationMessage],
    max_content_bytes: usize,
) -> Result<ContextProjection<'_>, ContextProjectionError> {
    let mut first = messages.len();
    let mut bytes = 0usize;

    for (index, message) in messages.iter().enumerate().rev() {
        let size = message.content.len();
        if size > max_content_bytes.saturating_sub(bytes) {
            if index == messages.len() - 1 {
                return Err(ContextProjectionError::CurrentMessageExceedsBudget {
                    content_bytes: size,
                    budget_bytes: max_content_bytes,
                });
            }
            break;
        }
        bytes += size;
        first = index;
    }

    Ok(ContextProjection {
        messages: &messages[first..],
        first_message_index: first,
        omitted_count: first,
        content_bytes: bytes,
    })
}
