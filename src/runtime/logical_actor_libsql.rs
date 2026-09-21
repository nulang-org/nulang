//! LibSQL-backed logical-actor persistence with authoritative epoch fencing.
//!
//! Unlike the legacy actor-id keyed persistence API, this backend stores the
//! full logical actor identity and performs the ownership predicate in the same
//! SQL statement as each durable mutation. This makes the database, not an
//! in-process preflight check, the stale-writer fence.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::{
    ActivationEpoch, ActivationHandle, ActorSnapshot, GrainId, JournalEntry,
    LogicalActorCommitError, LogicalActorCommitStamp, LogicalActorOwnershipError,
    LogicalActorOwnershipRecord, LogicalActorPersistenceStore, NodeId,
};

/// Errors from the LibSQL logical-actor backend.
#[derive(Debug)]
pub enum LibsqlLogicalActorError {
    Storage(String),
    Ownership(LogicalActorOwnershipError),
    Commit(LogicalActorCommitError),
    CorruptOwnership {
        grain_id: GrainId,
        detail: String,
    },
}

impl fmt::Display for LibsqlLogicalActorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(detail) => write!(f, "logical-actor storage error: {detail}"),
            Self::Ownership(error) => error.fmt(f),
            Self::Commit(error) => error.fmt(f),
            Self::CorruptOwnership { grain_id, detail } => write!(
                f,
                "corrupt logical-actor ownership record for {}: {}",
                grain_id.actor_name(),
                detail
            ),
        }
    }
}

impl std::error::Error for LibsqlLogicalActorError {}

impl From<LogicalActorOwnershipError> for LibsqlLogicalActorError {
    fn from(value: LogicalActorOwnershipError) -> Self {
        Self::Ownership(value)
    }
}

impl From<LogicalActorCommitError> for LibsqlLogicalActorError {
    fn from(value: LogicalActorCommitError) -> Self {
        Self::Commit(value)
    }
}

#[derive(Debug)]
struct PersistedOwnership {
    node_id: Option<NodeId>,
    activation_handle: Option<ActivationHandle>,
    epoch: ActivationEpoch,
}

/// Persistent logical-actor store backed by LibSQL/Turso.
///
/// Ownership epochs and node ids are encoded as fixed-width hexadecimal TEXT.
/// This preserves the complete `u64` range while retaining lexical ordering
/// for monotonic epoch comparisons in SQL.
pub struct LibsqlLogicalActorStore {
    conn: Mutex<libsql::Connection>,
    rt: tokio::runtime::Runtime,
    path: PathBuf,
}

impl LibsqlLogicalActorStore {
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let db_path = if path == Path::new(":memory:") {
            ":memory:".to_string()
        } else {
            path.to_string_lossy().into_owned()
        };
        let rt =
            tokio::runtime::Runtime::new().map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
        let db = rt.block_on(async {
            libsql::Builder::new_local(&db_path)
                .build()
                .await
                .map_err(storage_io)
        })?;
        let conn = db.connect().map_err(storage_io)?;
        let store = Self {
            conn: Mutex::new(conn),
            rt,
            path,
        };
        store.ensure_tables()?;
        Ok(store)
    }

    pub fn in_memory() -> io::Result<Self> {
        Self::new(":memory:")
    }

    pub fn new_remote(url: &str, auth_token: &str) -> io::Result<Self> {
        let rt =
            tokio::runtime::Runtime::new().map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
        let db = rt.block_on(async {
            libsql::Builder::new_remote(url.to_string(), auth_token.to_string())
                .build()
                .await
                .map_err(storage_io)
        })?;
        let conn = db.connect().map_err(storage_io)?;
        let store = Self {
            conn: Mutex::new(conn),
            rt,
            path: PathBuf::from(url),
        };
        store.ensure_tables()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> MutexGuard<'_, libsql::Connection> {
        self.conn.lock().unwrap()
    }

    fn ensure_tables(&self) -> io::Result<()> {
        let conn = self.conn();
        self.rt.block_on(async {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS logical_actor_ownership (
                    grain_type TEXT NOT NULL,
                    grain_key TEXT NOT NULL,
                    logical_id TEXT NOT NULL,
                    node_id TEXT,
                    activation_handle BLOB,
                    epoch TEXT NOT NULL,
                    PRIMARY KEY (grain_type, grain_key)
                )",
                (),
            )
            .await
            .map_err(storage_io)?;

            conn.execute(
                "CREATE TABLE IF NOT EXISTS logical_actor_snapshots (
                    grain_type TEXT NOT NULL,
                    grain_key TEXT NOT NULL,
                    sequence INTEGER NOT NULL,
                    snapshot TEXT NOT NULL,
                    PRIMARY KEY (grain_type, grain_key)
                )",
                (),
            )
            .await
            .map_err(storage_io)?;

            conn.execute(
                "CREATE TABLE IF NOT EXISTS logical_actor_journal (
                    grain_type TEXT NOT NULL,
                    grain_key TEXT NOT NULL,
                    sequence INTEGER NOT NULL,
                    entry TEXT NOT NULL,
                    PRIMARY KEY (grain_type, grain_key, sequence)
                )",
                (),
            )
            .await
            .map_err(storage_io)?;
            Ok(())
        })
    }

    /// Persist an authoritative ownership grant.
    ///
    /// The upsert accepts a strictly newer epoch or an exact grant replay.
    /// Equal-epoch conflicting owners and resurrection of a fenced epoch fail
    /// closed and are classified from the persisted record.
    pub fn grant_ownership(
        &mut self,
        grain_id: GrainId,
        node_id: NodeId,
        activation_handle: ActivationHandle,
        epoch: ActivationEpoch,
    ) -> Result<LogicalActorOwnershipRecord, LibsqlLogicalActorError> {
        let logical_id = grain_id.logical_id().to_string();
        let node = encode_u64(node_id.0);
        let epoch_text = encode_u64(epoch.get());
        let handle = activation_handle.get().to_be_bytes().to_vec();

        let conn = self.conn();
        let changed = self.rt.block_on(async {
            conn.execute(
                "INSERT INTO logical_actor_ownership
                    (grain_type, grain_key, logical_id, node_id, activation_handle, epoch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(grain_type, grain_key) DO UPDATE SET
                    logical_id = excluded.logical_id,
                    node_id = excluded.node_id,
                    activation_handle = excluded.activation_handle,
                    epoch = excluded.epoch
                 WHERE logical_actor_ownership.logical_id = excluded.logical_id
                   AND (
                        excluded.epoch > logical_actor_ownership.epoch
                        OR (
                            excluded.epoch = logical_actor_ownership.epoch
                            AND logical_actor_ownership.node_id = excluded.node_id
                            AND logical_actor_ownership.activation_handle = excluded.activation_handle
                        )
                   )",
                libsql::params![
                    grain_id.grain_type.as_str(),
                    grain_id.key.as_str(),
                    logical_id.as_str(),
                    node.as_str(),
                    handle,
                    epoch_text.as_str()
                ],
            )
            .await
            .map_err(storage_error)
        })?;
        drop(conn);

        if changed > 0 {
            let logical_actor_id = grain_id.logical_id();
            return Ok(LogicalActorOwnershipRecord {
                grain_id,
                logical_id: logical_actor_id,
                node_id,
                activation_handle,
                epoch,
            });
        }

        let current = self
            .read_persisted_ownership(&grain_id)?
            .ok_or_else(|| LibsqlLogicalActorError::CorruptOwnership {
                grain_id: grain_id.clone(),
                detail: "grant was rejected but no ownership row exists".to_string(),
            })?;

        if epoch < current.epoch {
            return Err(LogicalActorOwnershipError::StaleEpoch {
                known: current.epoch,
                attempted: epoch,
            }
            .into());
        }

        if epoch == current.epoch {
            return match (current.node_id, current.activation_handle) {
                (None, None) => {
                    Err(LogicalActorOwnershipError::EpochFenced { epoch }.into())
                }
                (Some(existing_node), Some(existing_handle)) => Err(
                    LogicalActorOwnershipError::EqualEpochConflict {
                        epoch,
                        existing_node,
                        existing_handle,
                        attempted_node: node_id,
                        attempted_handle: activation_handle,
                    }
                    .into(),
                ),
                _ => Err(LibsqlLogicalActorError::CorruptOwnership {
                    grain_id,
                    detail: "owner node and activation handle disagree on nullability".to_string(),
                }),
            };
        }

        Err(LibsqlLogicalActorError::CorruptOwnership {
            grain_id,
            detail: "newer ownership grant was unexpectedly rejected".to_string(),
        })
    }

    /// Fence ownership at an epoch, retaining a durable tombstone.
    pub fn fence_ownership(
        &mut self,
        grain_id: GrainId,
        epoch: ActivationEpoch,
    ) -> Result<(), LibsqlLogicalActorError> {
        let logical_id = grain_id.logical_id().to_string();
        let epoch_text = encode_u64(epoch.get());
        let conn = self.conn();
        let changed = self.rt.block_on(async {
            conn.execute(
                "INSERT INTO logical_actor_ownership
                    (grain_type, grain_key, logical_id, node_id, activation_handle, epoch)
                 VALUES (?1, ?2, ?3, NULL, NULL, ?4)
                 ON CONFLICT(grain_type, grain_key) DO UPDATE SET
                    logical_id = excluded.logical_id,
                    node_id = NULL,
                    activation_handle = NULL,
                    epoch = excluded.epoch
                 WHERE logical_actor_ownership.logical_id = excluded.logical_id
                   AND excluded.epoch >= logical_actor_ownership.epoch",
                libsql::params![
                    grain_id.grain_type.as_str(),
                    grain_id.key.as_str(),
                    logical_id.as_str(),
                    epoch_text.as_str()
                ],
            )
            .await
            .map_err(storage_error)
        })?;
        drop(conn);

        if changed > 0 {
            return Ok(());
        }

        let current = self
            .read_persisted_ownership(&grain_id)?
            .ok_or_else(|| LibsqlLogicalActorError::CorruptOwnership {
                grain_id: grain_id.clone(),
                detail: "fence was rejected but no ownership row exists".to_string(),
            })?;
        if epoch < current.epoch {
            return Err(LogicalActorOwnershipError::StaleEpoch {
                known: current.epoch,
                attempted: epoch,
            }
            .into());
        }
        if epoch == current.epoch && current.node_id.is_none() {
            return Ok(());
        }

        Err(LibsqlLogicalActorError::CorruptOwnership {
            grain_id,
            detail: "non-stale ownership fence was unexpectedly rejected".to_string(),
        })
    }

    pub fn ownership_record(
        &self,
        grain_id: &GrainId,
    ) -> Result<Option<LogicalActorOwnershipRecord>, LibsqlLogicalActorError> {
        let Some(current) = self.read_persisted_ownership(grain_id)? else {
            return Ok(None);
        };
        let (Some(node_id), Some(activation_handle)) =
            (current.node_id, current.activation_handle)
        else {
            return Ok(None);
        };
        Ok(Some(LogicalActorOwnershipRecord {
            grain_id: grain_id.clone(),
            logical_id: grain_id.logical_id(),
            node_id,
            activation_handle,
            epoch: current.epoch,
        }))
    }

    fn read_persisted_ownership(
        &self,
        grain_id: &GrainId,
    ) -> Result<Option<PersistedOwnership>, LibsqlLogicalActorError> {
        let expected_logical_id = grain_id.logical_id().to_string();
        let conn = self.conn();
        let row = self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT logical_id, node_id, activation_handle, epoch
                     FROM logical_actor_ownership
                     WHERE grain_type = ?1 AND grain_key = ?2",
                    libsql::params![grain_id.grain_type.as_str(), grain_id.key.as_str()],
                )
                .await
                .map_err(storage_error)?;
            rows.next().await.map_err(storage_error)
        })?;
        let Some(row) = row else {
            return Ok(None);
        };

        let stored_logical_id: String = row.get(0).map_err(storage_error)?;
        if stored_logical_id != expected_logical_id {
            return Err(LibsqlLogicalActorError::CorruptOwnership {
                grain_id: grain_id.clone(),
                detail: format!(
                    "logical-id collision/mismatch: stored {stored_logical_id}, expected {expected_logical_id}"
                ),
            });
        }

        let node_text: Option<String> = row.get(1).map_err(storage_error)?;
        let handle_raw: Option<Vec<u8>> = row.get(2).map_err(storage_error)?;
        let epoch_text: String = row.get(3).map_err(storage_error)?;
        let epoch_raw = decode_u64(&epoch_text).ok_or_else(|| {
            LibsqlLogicalActorError::CorruptOwnership {
                grain_id: grain_id.clone(),
                detail: format!("invalid epoch encoding {epoch_text:?}"),
            }
        })?;
        let epoch = ActivationEpoch::new(epoch_raw).ok_or_else(|| {
            LibsqlLogicalActorError::CorruptOwnership {
                grain_id: grain_id.clone(),
                detail: format!("invalid epoch value {epoch_raw}"),
            }
        })?;

        let node_id = match node_text {
            Some(value) => Some(NodeId(decode_u64(&value).ok_or_else(|| {
                LibsqlLogicalActorError::CorruptOwnership {
                    grain_id: grain_id.clone(),
                    detail: format!("invalid node-id encoding {value:?}"),
                }
            })?)),
            None => None,
        };
        let activation_handle = match handle_raw {
            Some(value) => {
                let bytes: [u8; 8] = value.as_slice().try_into().map_err(|_| {
                    LibsqlLogicalActorError::CorruptOwnership {
                        grain_id: grain_id.clone(),
                        detail: format!(
                            "invalid activation-handle encoding length {}",
                            value.len()
                        ),
                    }
                })?;
                let raw = u64::from_be_bytes(bytes);
                Some(ActivationHandle::new(raw).ok_or_else(|| {
                    LibsqlLogicalActorError::CorruptOwnership {
                        grain_id: grain_id.clone(),
                        detail: "activation handle must be non-zero".to_string(),
                    }
                })?)
            }
            None => None,
        };
        if node_id.is_some() != activation_handle.is_some() {
            return Err(LibsqlLogicalActorError::CorruptOwnership {
                grain_id: grain_id.clone(),
                detail: "owner node and activation handle disagree on nullability".to_string(),
            });
        }

        Ok(Some(PersistedOwnership {
            node_id,
            activation_handle,
            epoch,
        }))
    }

    fn unauthorized(
        &self,
        stamp: &LogicalActorCommitStamp,
    ) -> Result<LibsqlLogicalActorError, LibsqlLogicalActorError> {
        Ok(LogicalActorCommitError::Unauthorized {
            grain_id: stamp.grain_id.clone(),
            node_id: stamp.node_id,
            epoch: stamp.epoch,
            current: self.ownership_record(&stamp.grain_id)?,
        }
        .into())
    }

    fn journal_entry_json(
        &self,
        grain_id: &GrainId,
        sequence: u64,
    ) -> Result<Option<String>, LibsqlLogicalActorError> {
        let sequence = i64::try_from(sequence).map_err(|_| {
            LibsqlLogicalActorError::Storage(
                "journal sequence exceeds LibSQL signed integer range".to_string(),
            )
        })?;
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT entry FROM logical_actor_journal
                     WHERE grain_type = ?1 AND grain_key = ?2 AND sequence = ?3",
                    libsql::params![
                        grain_id.grain_type.as_str(),
                        grain_id.key.as_str(),
                        sequence
                    ],
                )
                .await
                .map_err(storage_error)?;
            let Some(row) = rows.next().await.map_err(storage_error)? else {
                return Ok(None);
            };
            row.get(0).map(Some).map_err(storage_error)
        })
    }

    fn current_snapshot_sequence(
        &self,
        grain_id: &GrainId,
    ) -> Result<Option<u64>, LibsqlLogicalActorError> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT sequence FROM logical_actor_snapshots
                     WHERE grain_type = ?1 AND grain_key = ?2",
                    libsql::params![grain_id.grain_type.as_str(), grain_id.key.as_str()],
                )
                .await
                .map_err(storage_error)?;
            let Some(row) = rows.next().await.map_err(storage_error)? else {
                return Ok(None);
            };
            let sequence: i64 = row.get(0).map_err(storage_error)?;
            Ok(Some(sequence.max(0) as u64))
        })
    }
}

impl LogicalActorPersistenceStore for LibsqlLogicalActorStore {
    type Error = LibsqlLogicalActorError;

    fn save_logical_snapshot(
        &mut self,
        stamp: &LogicalActorCommitStamp,
        snapshot: ActorSnapshot,
    ) -> Result<(), Self::Error> {
        let snapshot_json = serde_json::to_string(&snapshot).map_err(storage_error)?;
        let node = encode_u64(stamp.node_id.0);
        let epoch = encode_u64(stamp.epoch.get());
        let sequence = i64::try_from(snapshot.sequence).map_err(|_| {
            LibsqlLogicalActorError::Storage(
                "snapshot sequence exceeds LibSQL signed integer range".to_string(),
            )
        })?;

        let conn = self.conn();
        let changed = self.rt.block_on(async {
            conn.execute(
                "INSERT INTO logical_actor_snapshots
                    (grain_type, grain_key, sequence, snapshot)
                 SELECT ?1, ?2, ?3, ?4
                 WHERE EXISTS (
                    SELECT 1 FROM logical_actor_ownership
                    WHERE grain_type = ?1
                      AND grain_key = ?2
                      AND node_id = ?5
                      AND epoch = ?6
                 )
                 ON CONFLICT(grain_type, grain_key) DO UPDATE SET
                    sequence = excluded.sequence,
                    snapshot = excluded.snapshot
                 WHERE excluded.sequence >= logical_actor_snapshots.sequence",
                libsql::params![
                    stamp.grain_id.grain_type.as_str(),
                    stamp.grain_id.key.as_str(),
                    sequence,
                    snapshot_json.as_str(),
                    node.as_str(),
                    epoch.as_str()
                ],
            )
            .await
            .map_err(storage_error)
        })?;
        drop(conn);

        if changed > 0 {
            return Ok(());
        }

        let current = self.ownership_record(&stamp.grain_id)?;
        let authorized = current.as_ref().is_some_and(|record| {
            record.node_id == stamp.node_id && record.epoch == stamp.epoch
        });
        if !authorized {
            return Err(self.unauthorized(stamp)?);
        }

        if let Some(current_sequence) = self.current_snapshot_sequence(&stamp.grain_id)? {
            return Err(LogicalActorCommitError::StaleSnapshotSequence {
                grain_id: stamp.grain_id.clone(),
                current: current_sequence,
                attempted: snapshot.sequence,
            }
            .into());
        }

        Err(LibsqlLogicalActorError::Storage(
            "authorized logical-actor snapshot write affected no rows".to_string(),
        ))
    }

    fn load_logical_snapshot(
        &self,
        grain_id: &GrainId,
    ) -> Result<Option<ActorSnapshot>, Self::Error> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT snapshot FROM logical_actor_snapshots
                     WHERE grain_type = ?1 AND grain_key = ?2",
                    libsql::params![grain_id.grain_type.as_str(), grain_id.key.as_str()],
                )
                .await
                .map_err(storage_error)?;
            let Some(row) = rows.next().await.map_err(storage_error)? else {
                return Ok(None);
            };
            let json: String = row.get(0).map_err(storage_error)?;
            let snapshot = serde_json::from_str(&json).map_err(storage_error)?;
            Ok(Some(snapshot))
        })
    }

    fn append_logical_journal(
        &mut self,
        stamp: &LogicalActorCommitStamp,
        entry: JournalEntry,
    ) -> Result<(), Self::Error> {
        let entry_json = serde_json::to_string(&entry).map_err(storage_error)?;
        let node = encode_u64(stamp.node_id.0);
        let epoch = encode_u64(stamp.epoch.get());
        let sequence = i64::try_from(entry.sequence).map_err(|_| {
            LibsqlLogicalActorError::Storage(
                "journal sequence exceeds LibSQL signed integer range".to_string(),
            )
        })?;

        let conn = self.conn();
        let changed = self.rt.block_on(async {
            conn.execute(
                "INSERT INTO logical_actor_journal
                    (grain_type, grain_key, sequence, entry)
                 SELECT ?1, ?2, ?3, ?4
                 WHERE EXISTS (
                    SELECT 1 FROM logical_actor_ownership
                    WHERE grain_type = ?1
                      AND grain_key = ?2
                      AND node_id = ?5
                      AND epoch = ?6
                 )
                 ON CONFLICT(grain_type, grain_key, sequence) DO UPDATE SET
                    entry = excluded.entry
                 WHERE logical_actor_journal.entry = excluded.entry",
                libsql::params![
                    stamp.grain_id.grain_type.as_str(),
                    stamp.grain_id.key.as_str(),
                    sequence,
                    entry_json.as_str(),
                    node.as_str(),
                    epoch.as_str()
                ],
            )
            .await
            .map_err(storage_error)
        })?;
        drop(conn);

        if changed > 0 {
            return Ok(());
        }

        let current = self.ownership_record(&stamp.grain_id)?;
        let authorized = current.as_ref().is_some_and(|record| {
            record.node_id == stamp.node_id && record.epoch == stamp.epoch
        });
        if !authorized {
            return Err(self.unauthorized(stamp)?);
        }

        if let Some(existing) = self.journal_entry_json(&stamp.grain_id, entry.sequence)? {
            if existing == entry_json {
                return Ok(());
            }
            return Err(LogicalActorCommitError::ConflictingJournalSequence {
                grain_id: stamp.grain_id.clone(),
                sequence: entry.sequence,
            }
            .into());
        }

        Err(LibsqlLogicalActorError::Storage(
            "authorized logical-actor journal write affected no rows".to_string(),
        ))
    }

    fn read_logical_journal(
        &self,
        grain_id: &GrainId,
    ) -> Result<Vec<JournalEntry>, Self::Error> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT entry FROM logical_actor_journal
                     WHERE grain_type = ?1 AND grain_key = ?2
                     ORDER BY sequence ASC",
                    libsql::params![grain_id.grain_type.as_str(), grain_id.key.as_str()],
                )
                .await
                .map_err(storage_error)?;
            let mut entries = Vec::new();
            while let Some(row) = rows.next().await.map_err(storage_error)? {
                let json: String = row.get(0).map_err(storage_error)?;
                entries.push(serde_json::from_str(&json).map_err(storage_error)?);
            }
            Ok(entries)
        })
    }
}

fn encode_u64(value: u64) -> String {
    format!("{value:016x}")
}

fn decode_u64(value: &str) -> Option<u64> {
    if value.len() != 16 {
        return None;
    }
    u64::from_str_radix(value, 16).ok()
}

fn storage_error(error: impl fmt::Display) -> LibsqlLogicalActorError {
    LibsqlLogicalActorError::Storage(error.to_string())
}

fn storage_io(error: impl fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::PersistedValue;

    fn handle(raw: u64) -> ActivationHandle {
        ActivationHandle::new(raw).unwrap()
    }

    fn epoch(raw: u64) -> ActivationEpoch {
        ActivationEpoch::new(raw).unwrap()
    }

    fn snapshot(actor_id: u64, sequence: u64, value: i64) -> ActorSnapshot {
        let mut snapshot = ActorSnapshot {
            actor_id,
            sequence,
            ..ActorSnapshot::default()
        };
        snapshot
            .state
            .insert("value".to_string(), PersistedValue::Int(value));
        snapshot
    }

    #[test]
    fn stale_owner_cannot_commit_after_persisted_handoff() {
        let mut store = LibsqlLogicalActorStore::in_memory().unwrap();
        let grain = GrainId::new("Account", "libsql-handoff");
        store
            .grant_ownership(grain.clone(), NodeId(1), handle(10), epoch(1))
            .unwrap();

        let old = LogicalActorCommitStamp::new(grain.clone(), NodeId(1), epoch(1));
        store
            .save_logical_snapshot(&old, snapshot(100, 1, 10))
            .unwrap();

        store
            .grant_ownership(grain.clone(), NodeId(2), handle(20), epoch(2))
            .unwrap();

        assert!(matches!(
            store.save_logical_snapshot(&old, snapshot(100, 2, 99)),
            Err(LibsqlLogicalActorError::Commit(
                LogicalActorCommitError::Unauthorized { .. }
            ))
        ));

        let current = LogicalActorCommitStamp::new(grain.clone(), NodeId(2), epoch(2));
        store
            .save_logical_snapshot(&current, snapshot(200, 2, 20))
            .unwrap();
        assert_eq!(
            store
                .load_logical_snapshot(&grain)
                .unwrap()
                .unwrap()
                .actor_id,
            200
        );
    }

    #[test]
    fn fence_blocks_commits_until_a_newer_grant() {
        let mut store = LibsqlLogicalActorStore::in_memory().unwrap();
        let grain = GrainId::new("Account", "libsql-fence");
        store
            .grant_ownership(grain.clone(), NodeId(1), handle(1), epoch(3))
            .unwrap();
        let stale = LogicalActorCommitStamp::new(grain.clone(), NodeId(1), epoch(3));

        store.fence_ownership(grain.clone(), epoch(4)).unwrap();
        assert!(matches!(
            store.save_logical_snapshot(&stale, snapshot(1, 1, 1)),
            Err(LibsqlLogicalActorError::Commit(
                LogicalActorCommitError::Unauthorized { .. }
            ))
        ));

        store
            .grant_ownership(grain.clone(), NodeId(2), handle(2), epoch(5))
            .unwrap();
        let current = LogicalActorCommitStamp::new(grain, NodeId(2), epoch(5));
        store
            .save_logical_snapshot(&current, snapshot(2, 1, 2))
            .unwrap();
    }

    #[test]
    fn full_u64_activation_handle_roundtrips_without_narrowing() {
        let mut store = LibsqlLogicalActorStore::in_memory().unwrap();
        let grain = GrainId::new("Account", "u64-handle");
        let max_handle = ActivationHandle::new(u64::MAX).unwrap();

        store
            .grant_ownership(grain.clone(), NodeId(1), max_handle, epoch(1))
            .unwrap();

        let record = store.ownership_record(&grain).unwrap().unwrap();
        assert_eq!(record.activation_handle, max_handle);

        // Exact replay at the same epoch remains idempotent even at the top
        // of the u64 activation-handle range.
        store
            .grant_ownership(grain, NodeId(1), max_handle, epoch(1))
            .unwrap();
    }

    #[test]
    fn full_u64_epoch_order_is_preserved() {
        let mut store = LibsqlLogicalActorStore::in_memory().unwrap();
        let grain = GrainId::new("Account", "u64-epoch");
        let high = ActivationEpoch::new(u64::MAX - 1).unwrap();
        store
            .grant_ownership(grain.clone(), NodeId(u64::MAX), handle(1), high)
            .unwrap();

        assert!(matches!(
            store.grant_ownership(
                grain,
                NodeId(1),
                handle(2),
                ActivationEpoch::new(i64::MAX as u64).unwrap()
            ),
            Err(LibsqlLogicalActorError::Ownership(
                LogicalActorOwnershipError::StaleEpoch { .. }
            ))
        ));
    }

    #[test]
    fn ownership_fence_survives_store_reopen() {
        let path = std::env::temp_dir().join(format!(
            "nulang_logical_actor_fencing_{}_reopen.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let grain = GrainId::new("Account", "persisted-ownership");
        let stale = LogicalActorCommitStamp::new(grain.clone(), NodeId(7), epoch(10));

        {
            let mut store = LibsqlLogicalActorStore::new(&path).unwrap();
            store
                .grant_ownership(grain.clone(), NodeId(7), handle(1), epoch(10))
                .unwrap();
            store
                .save_logical_snapshot(&stale, snapshot(1, 1, 10))
                .unwrap();
        }

        {
            let mut store = LibsqlLogicalActorStore::new(&path).unwrap();
            store
                .grant_ownership(grain.clone(), NodeId(8), handle(2), epoch(11))
                .unwrap();

            assert!(matches!(
                store.save_logical_snapshot(&stale, snapshot(1, 2, 99)),
                Err(LibsqlLogicalActorError::Commit(
                    LogicalActorCommitError::Unauthorized { .. }
                ))
            ));

            let current =
                LogicalActorCommitStamp::new(grain.clone(), NodeId(8), epoch(11));
            store
                .save_logical_snapshot(&current, snapshot(2, 2, 20))
                .unwrap();
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn journal_replay_is_idempotent_but_conflicting_payload_fails_closed() {
        let mut store = LibsqlLogicalActorStore::in_memory().unwrap();
        let grain = GrainId::new("Account", "libsql-journal-idempotence");
        store
            .grant_ownership(grain.clone(), NodeId(1), handle(1), epoch(1))
            .unwrap();
        let stamp = LogicalActorCommitStamp::new(grain.clone(), NodeId(1), epoch(1));
        let first = JournalEntry {
            sequence: 1,
            behavior_id: 0,
            payload: vec![PersistedValue::Int(1)],
        };

        store.append_logical_journal(&stamp, first.clone()).unwrap();
        store.append_logical_journal(&stamp, first).unwrap();
        assert_eq!(store.read_logical_journal(&grain).unwrap().len(), 1);

        let conflict = JournalEntry {
            sequence: 1,
            behavior_id: 1,
            payload: vec![PersistedValue::Int(2)],
        };
        assert!(matches!(
            store.append_logical_journal(&stamp, conflict),
            Err(LibsqlLogicalActorError::Commit(
                LogicalActorCommitError::ConflictingJournalSequence {
                    sequence: 1,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn snapshot_sequence_cannot_regress() {
        let mut store = LibsqlLogicalActorStore::in_memory().unwrap();
        let grain = GrainId::new("Account", "libsql-sequence");
        store
            .grant_ownership(grain.clone(), NodeId(1), handle(1), epoch(1))
            .unwrap();
        let stamp = LogicalActorCommitStamp::new(grain.clone(), NodeId(1), epoch(1));

        store
            .save_logical_snapshot(&stamp, snapshot(1, 10, 10))
            .unwrap();
        assert!(matches!(
            store.save_logical_snapshot(&stamp, snapshot(1, 9, 9)),
            Err(LibsqlLogicalActorError::Commit(
                LogicalActorCommitError::StaleSnapshotSequence {
                    current: 10,
                    attempted: 9,
                    ..
                }
            ))
        ));
    }
}
