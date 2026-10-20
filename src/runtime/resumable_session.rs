//! Transport-neutral, bounded resumable output sessions.
//!
//! This is a **session protocol primitive**, not a terminal server, network
//! listener, durable actor inbox, or exactly-once delivery implementation.
//! An authenticated gateway must supply trusted tenant/principal claims and
//! server-generated unpredictable session IDs. Acknowledgements describe
//! client-observed output, never an application side-effect commit.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, VecDeque};
use std::fmt;

pub const RESUMABLE_SESSION_PROTOCOL_VERSION: &str = "nulang-session/1";

/// Wire-safe monotonic counter: decimal JSON string, never an IEEE-754 number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionCursor(pub u64);

impl Serialize for SessionCursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for SessionCursor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let value = text.parse::<u64>().map_err(serde::de::Error::custom)?;
        if text != value.to_string() {
            return Err(serde::de::Error::custom("noncanonical session cursor"));
        }
        Ok(Self(value))
    }
}

/// A handshake marker that refuses unknown protocol versions during decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionProtocolVersion;

impl Serialize for SessionProtocolVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(RESUMABLE_SESSION_PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for SessionProtocolVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == RESUMABLE_SESSION_PROTOCOL_VERSION {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("unsupported resumable session protocol version"))
        }
    }
}

/// Client messages are **not** authentication credentials. The gateway must
/// validate access separately before invoking the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionClientMessage {
    Open {
        version: SessionProtocolVersion,
        session_id: String,
    },
    Resume {
        version: SessionProtocolVersion,
        session_id: String,
        after: SessionCursor,
    },
    Ack {
        session_id: String,
        epoch: SessionCursor,
        cursor: SessionCursor,
    },
    Close {
        session_id: String,
        epoch: SessionCursor,
    },
}

/// Byte payloads are represented as JSON arrays for portability in v1.
/// Gateways should enforce a serialized-frame limit before deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionServerMessage {
    Ready {
        session_id: String,
        epoch: SessionCursor,
        latest: SessionCursor,
    },
    Output {
        session_id: String,
        epoch: SessionCursor,
        sequence: SessionCursor,
        bytes: Vec<u8>,
    },
    Gap {
        session_id: String,
        epoch: SessionCursor,
        requested: SessionCursor,
        first_available: SessionCursor,
        latest: SessionCursor,
    },
    Closed {
        session_id: String,
    },
}

/// Obtained from verified authorization claims, never from client-provided
/// session data. A tenant and principal must both match when resuming.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionIdentity {
    tenant_id: String,
    principal_id: String,
}

impl SessionIdentity {
    pub fn new(
        tenant_id: impl Into<String>,
        principal_id: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let tenant_id = tenant_id.into();
        let principal_id = principal_id.into();
        if tenant_id.trim().is_empty() || principal_id.trim().is_empty() {
            return Err(SessionError::InvalidIdentity);
        }
        Ok(Self {
            tenant_id,
            principal_id,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    InvalidConfig,
    InvalidIdentity,
    AlreadyExists,
    NotFound,
    StaleEpoch,
    EpochExhausted,
    SequenceExhausted,
    CursorAhead,
    CursorRegression,
    FrameTooLarge,
    EmptyFrame,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReady {
    pub epoch: SessionCursor,
    pub latest: SessionCursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOutput {
    pub sequence: SessionCursor,
    pub bytes: Vec<u8>,
}

/// A gap is not a successful replay. The caller must obtain an authoritative
/// application snapshot or start a new logical session before proceeding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReplay {
    Replayed {
        epoch: SessionCursor,
        latest: SessionCursor,
        frames: Vec<SessionOutput>,
    },
    Gap {
        epoch: SessionCursor,
        requested: SessionCursor,
        first_available: SessionCursor,
        latest: SessionCursor,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRecord {
    owner: SessionIdentity,
    epoch: SessionCursor,
    latest: SessionCursor,
    acknowledged: SessionCursor,
    /// Largest cursor actually offered by this gateway connection epoch.
    /// An ACK never proves effects were executed or output was processed.
    served_through: SessionCursor,
    history: VecDeque<SessionOutput>,
    history_bytes: usize,
}

/// In-memory reference state machine for authenticated resumable output.
/// A process restart loses these sessions. Persistent session metadata and a
/// Fabric Streams backing adapter are separate, intentionally unclaimed work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRegistry {
    max_frames: usize,
    max_bytes: usize,
    sessions: HashMap<String, SessionRecord>,
}

impl SessionRegistry {
    pub fn new(max_frames: usize, max_bytes: usize) -> Result<Self, SessionError> {
        if max_frames == 0 || max_bytes == 0 {
            return Err(SessionError::InvalidConfig);
        }
        Ok(Self {
            max_frames,
            max_bytes,
            sessions: HashMap::new(),
        })
    }

    /// Only a trusted server may mint an ID and open a session. Existing IDs
    /// are never rebound, even if the owner would otherwise match.
    pub fn open(
        &mut self,
        session_id: &str,
        owner: SessionIdentity,
    ) -> Result<SessionReady, SessionError> {
        if session_id.trim().is_empty() {
            return Err(SessionError::InvalidIdentity);
        }
        if self.sessions.contains_key(session_id) {
            return Err(SessionError::AlreadyExists);
        }
        let initial = SessionCursor(0);
        let epoch = SessionCursor(1);
        self.sessions.insert(
            session_id.to_owned(),
            SessionRecord {
                owner,
                epoch,
                latest: initial,
                acknowledged: initial,
                served_through: initial,
                history: VecDeque::new(),
                history_bytes: 0,
            },
        );
        Ok(SessionReady { epoch, latest: initial })
    }

    /// Trusted producer path: appends output only, never reruns an actor turn
    /// or an external side effect. Output sequence numbers are session-local.
    pub fn append_output(
        &mut self,
        session_id: &str,
        bytes: Vec<u8>,
    ) -> Result<SessionCursor, SessionError> {
        if bytes.is_empty() {
            return Err(SessionError::EmptyFrame);
        }
        if bytes.len() > self.max_bytes {
            return Err(SessionError::FrameTooLarge);
        }
        let record = self.sessions.get_mut(session_id).ok_or(SessionError::NotFound)?;
        let next = record.latest.0.checked_add(1).ok_or(SessionError::SequenceExhausted)?;
        while record.history.len() >= self.max_frames
            || record.history_bytes > self.max_bytes - bytes.len()
        {
            let discarded = record.history.pop_front().expect("bounded buffer has prior frames");
            record.history_bytes -= discarded.bytes.len();
        }
        record.history_bytes += bytes.len();
        record.latest = SessionCursor(next);
        record.history.push_back(SessionOutput {
            sequence: record.latest,
            bytes,
        });
        Ok(record.latest)
    }

    /// Rotate the connection epoch. The old connection is fenced even if a
    /// retention gap prevents catch-up.
    pub fn resume(
        &mut self,
        session_id: &str,
        caller: &SessionIdentity,
        after: SessionCursor,
    ) -> Result<SessionReplay, SessionError> {
        let record = self.access_mut(session_id, caller)?;
        if after > record.latest {
            return Err(SessionError::CursorAhead);
        }
        let next = record.epoch.0.checked_add(1).ok_or(SessionError::EpochExhausted)?;
        record.epoch = SessionCursor(next);
        record.served_through = SessionCursor(0);
        let result = Self::replay(record, after);
        if let SessionReplay::Replayed { latest, .. } = result {
            record.served_through = latest;
        }
        Ok(result)
    }

    /// Poll output on the *current* connection. Stale connection generations
    /// cannot read or acknowledge data after a successful resumption.
    pub fn read_after(
        &mut self,
        session_id: &str,
        caller: &SessionIdentity,
        epoch: SessionCursor,
        after: SessionCursor,
    ) -> Result<SessionReplay, SessionError> {
        let record = self.access_mut(session_id, caller)?;
        if epoch != record.epoch {
            return Err(SessionError::StaleEpoch);
        }
        if after > record.latest {
            return Err(SessionError::CursorAhead);
        }
        let result = Self::replay(record, after);
        if let SessionReplay::Replayed { latest, .. } = result {
            record.served_through = record.served_through.max(latest);
        }
        Ok(result)
    }

    /// An ACK advances an advisory cursor, not a durable inbox, transaction,
    /// or replicated Fabric commit position.
    pub fn ack(
        &mut self,
        session_id: &str,
        caller: &SessionIdentity,
        epoch: SessionCursor,
        cursor: SessionCursor,
    ) -> Result<(), SessionError> {
        let record = self.access_mut(session_id, caller)?;
        if epoch != record.epoch {
            return Err(SessionError::StaleEpoch);
        }
        if cursor > record.served_through {
            return Err(SessionError::CursorAhead);
        }
        if cursor < record.acknowledged {
            return Err(SessionError::CursorRegression);
        }
        record.acknowledged = cursor;
        Ok(())
    }

    pub fn close(
        &mut self,
        session_id: &str,
        caller: &SessionIdentity,
        epoch: SessionCursor,
    ) -> Result<(), SessionError> {
        let record = self.access(session_id, caller)?;
        if epoch != record.epoch {
            return Err(SessionError::StaleEpoch);
        }
        self.sessions.remove(session_id);
        Ok(())
    }

    fn access<'a>(
        &'a self,
        session_id: &str,
        caller: &SessionIdentity,
    ) -> Result<&'a SessionRecord, SessionError> {
        match self.sessions.get(session_id) {
            Some(record) if &record.owner == caller => Ok(record),
            // Deliberately indistinguishable: avoid leaking other tenants' IDs.
            _ => Err(SessionError::NotFound),
        }
    }

    fn access_mut<'a>(
        &'a mut self,
        session_id: &str,
        caller: &SessionIdentity,
    ) -> Result<&'a mut SessionRecord, SessionError> {
        match self.sessions.get_mut(session_id) {
            Some(record) if &record.owner == caller => Ok(record),
            _ => Err(SessionError::NotFound),
        }
    }

    fn replay(record: &SessionRecord, after: SessionCursor) -> SessionReplay {
        let first_available = record
            .history
            .front()
            .map(|entry| entry.sequence)
            .unwrap_or(SessionCursor(1));
        if after.0 < first_available.0.saturating_sub(1) {
            return SessionReplay::Gap {
                epoch: record.epoch,
                requested: after,
                first_available,
                latest: record.latest,
            };
        }
        SessionReplay::Replayed {
            epoch: record.epoch,
            latest: record.latest,
            frames: record.history.iter()
                .filter(|entry| entry.sequence > after)
                .cloned()
                .collect(),
        }
    }
}
