//! Versioned, replay-stable identity for inbound durable actor commands.
//!
//! This does not retrofit the existing message wire format or turn journal.
//! A trusted host must supply and preserve the logical sender/message keys.
//! Persistence backends must atomically bind the identity to the accepted
//! transition or return Unsupported; a read followed by an ordinary commit
//! is not a safe deduplication implementation.

use std::fmt;
use std::io;

use blake3::Hasher;

use crate::runtime::{DurableCommit, JournalEntry, PersistedValue};

const INBOX_ID_DOMAIN: &[u8] = b"nulang.durable-inbox-id.v1\0";
const INBOX_REQUEST_DOMAIN: &[u8] = b"nulang.durable-inbox-request.v1\0";

/// Stable identity of one *logical* message to one durable actor.
///
/// Receiver activation epochs and receiver commit sequences are deliberately
/// not included: retries following placement changes must retain identity.
/// The caller-supplied sender and message key must be stable across retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurableInboxId([u8; 32]);

impl DurableInboxId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for DurableInboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Trusted-host descriptor of one incoming command.
///
/// Identity is derived from namespace, sender, message key and receiver.
/// The request digest separately commits to actor, behavior and canonical
/// payload; two different requests cannot silently reuse the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableInboxCommand {
    id: DurableInboxId,
    actor_id: u64,
    behavior_id: u16,
    request_digest: [u8; 32],
}

impl DurableInboxCommand {
    pub fn new(
        namespace: &str,
        sender: &str,
        logical_message_key: &str,
        actor_id: u64,
        behavior_id: u16,
        payload: &[PersistedValue],
    ) -> io::Result<Self> {
        for (label, part) in [
            ("namespace", namespace),
            ("sender", sender),
            ("logical message key", logical_message_key),
        ] {
            if part.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("durable inbox {label} must not be empty"),
                ));
            }
        }
        if actor_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable inbox receiver actor id must be non-zero",
            ));
        }

        let mut hasher = Hasher::new();
        hasher.update(INBOX_ID_DOMAIN);
        hash_part(&mut hasher, namespace.as_bytes());
        hash_part(&mut hasher, sender.as_bytes());
        hash_part(&mut hasher, logical_message_key.as_bytes());
        hasher.update(&actor_id.to_le_bytes());
        let id = DurableInboxId(*hasher.finalize().as_bytes());

        Ok(Self {
            id,
            actor_id,
            behavior_id,
            request_digest: digest_request(actor_id, behavior_id, payload)?,
        })
    }

    pub fn id(&self) -> DurableInboxId {
        self.id
    }

    pub fn request_digest(&self) -> [u8; 32] {
        self.request_digest
    }

    /// Check against the *actual* journal candidate before any storage
    /// mutation. Metadata from an untrusted host is never enough by itself.
    pub fn validate_command(&self, actor_id: u64, command: &JournalEntry) -> io::Result<()> {
        if actor_id != self.actor_id || command.behavior_id != self.behavior_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable inbox metadata does not match the actor or behavior",
            ));
        }
        if digest_request(actor_id, command.behavior_id, &command.payload)?
            != self.request_digest
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable inbox request digest does not match the journal payload",
            ));
        }
        Ok(())
    }
}

/// Storage-returned admission disposition. Only a newly accepted command may
/// be handed to a user handler. A replayed receipt is an ACK, not work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableInboxCommitOutcome {
    pub commit: DurableCommit,
    pub newly_accepted: bool,
}

/// Receipt recorded together with an accepted transition.
///
/// An equal logical identity with an unequal request digest is a conflict,
/// never an instruction to overwrite or silently dispatch again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableInboxReceipt {
    pub id: DurableInboxId,
    pub request_digest: [u8; 32],
    pub commit: DurableCommit,
}

fn hash_part(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn digest_request(
    actor_id: u64,
    behavior_id: u16,
    payload: &[PersistedValue],
) -> io::Result<[u8; 32]> {
    // Non-finite floats have no portable cross-runtime canonical meaning.
    // Refuse them rather than collapsing distinct values into an identity.
    if payload
        .iter()
        .any(|value| matches!(value, PersistedValue::Float(number) if !number.is_finite()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "non-finite float is not a canonical durable inbox value",
        ));
    }
    // Stable versioned binary encoding: do not derive persistent identity
    // from serde_json's evolving float formatting or fallback null encoding.
    // Tag each variant explicitly and length-prefix variable-width fields.
    let mut hasher = Hasher::new();
    hasher.update(INBOX_REQUEST_DOMAIN);
    hasher.update(&actor_id.to_le_bytes());
    hasher.update(&behavior_id.to_le_bytes());
    hasher.update(&(payload.len() as u64).to_le_bytes());
    for value in payload {
        match value {
            PersistedValue::Int(number) => {
                hasher.update(&[1]);
                hasher.update(&number.to_le_bytes());
            }
            PersistedValue::Float(number) => {
                hasher.update(&[2]);
                hasher.update(&number.to_bits().to_le_bytes());
            }
            PersistedValue::Bool(value) => {
                hasher.update(&[3, u8::from(*value)]);
            }
            PersistedValue::String(value) => {
                hasher.update(&[4]);
                hash_part(&mut hasher, value.as_bytes());
            }
            PersistedValue::Nil => {
                hasher.update(&[5]);
            }
            PersistedValue::Unit => {
                hasher.update(&[6]);
            }
            PersistedValue::Actor(id) => {
                hasher.update(&[7]);
                hasher.update(&id.to_le_bytes());
            }
        }
    }
    Ok(*hasher.finalize().as_bytes())
}
