//! Test-only rusqlite atomic durable writer on *pre-existing* local databases.
//!
//! This never implements the production PersistenceStore. Native SQLite is
//! isolated in a separate process from the old libSQL runtime. SQL failures
//! roll back the entire transaction before they can be reported as success.
use std::{io, path::Path, time::Duration};

use nulang::runtime::{DurableCommit, DurableTransition};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};

use super::RusqliteRecoveryReader;

fn failure(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}
fn invalid(text: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.to_string())
}
fn bounded(value: u64) -> io::Result<i64> {
    i64::try_from(value).map_err(|_| invalid("durable SQLite integer out of range"))
}
fn json<T: serde::Serialize>(value: &T) -> io::Result<String> {
    serde_json::to_string(value).map_err(invalid)
}

pub struct RusqliteAtomicWriter {
    connection: Connection,
}

impl RusqliteAtomicWriter {
    /// Only open an already-created, current-schema SQLite database.
    /// No implicit CREATE, ALTER, migrations or production backend changes.
    pub fn open_existing(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let proof = RusqliteRecoveryReader::open(path)?;
        drop(proof);
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(failure)?;
        conn.busy_timeout(Duration::from_secs(10))
            .map_err(failure)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(failure)?;
        let sync: i64 = conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .map_err(failure)?;
        if sync != 2 {
            return Err(invalid(
                "synchronous FULL must be enabled before durable writes",
            ));
        }
        let journal: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(failure)?;
        if !journal.eq_ignore_ascii_case("wal") {
            return Err(invalid(
                "only existing WAL-mode SQLite durable stores are supported",
            ));
        }
        Ok(Self { connection: conn })
    }

    /// Bound the current SQLite pager to its existing page count, for
    /// *test-only* deterministic SQLITE_FULL injection. This does not fill
    /// the shared physical filesystem, change a production migration, or
    /// simulate power loss. It affects this writer connection only.
    pub fn limit_pages_to_current_for_testing(&mut self) -> io::Result<i64> {
        let pages: i64 = self
            .connection
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .map_err(failure)?;
        if pages <= 0 {
            return Err(invalid("database must contain existing SQLite pages"));
        }
        // Only the SQLite-reported positive integer is interpolated.
        let effective: i64 = self
            .connection
            .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |r| {
                r.get(0)
            })
            .map_err(failure)?;
        if effective != pages {
            return Err(invalid("failed to enforce test database page limit"));
        }
        Ok(effective)
    }

    /// All eight durable tables plus legacy snapshot/journal are committed
    /// inside one SQLite BEGIN IMMEDIATE transaction. Fail closed on
    /// idempotency conflicts, missing ancestors, stale epochs and CAS races.
    pub fn commit(&mut self, transition: &DurableTransition) -> io::Result<DurableCommit> {
        let digest = transition.digest()?; // structural validation, canonical identity
        let digest_hex = digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let actor = bounded(transition.actor_id)?;
        let epoch = bounded(transition.activation_epoch)?;
        let seq = bounded(transition.sequence)?;
        let predecessor = bounded(transition.expected_previous_sequence)?;

        // Pre-serialize everything before holding any write lock.
        let snapshot_data = transition
            .snapshot
            .as_ref()
            .map(|s| {
                Ok::<_, io::Error>((
                    json(&s.state)?,
                    json(&s.crdt_snapshot)?,
                    json(&s.crdt_field_map)?,
                    json(&s.authority_tokens)?,
                ))
            })
            .transpose()?;
        let command_json = transition
            .command
            .as_ref()
            .map(|c| json(&c.payload))
            .transpose()?;
        let workflow_json = transition
            .workflow_events
            .iter()
            .map(json)
            .collect::<io::Result<Vec<_>>>()?;
        let domain_json = transition
            .domain_events
            .iter()
            .map(json)
            .collect::<io::Result<Vec<_>>>()?;
        let effect_json = transition
            .durable_effects
            .iter()
            .map(|record| {
                let b = record.to_json().map_err(invalid)?;
                String::from_utf8(b).map_err(invalid)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let outbox_json = transition
            .outbox
            .iter()
            .map(|m| json(&m.payload))
            .collect::<io::Result<Vec<_>>>()?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failure)?;
        let current: Option<(i64, i64, String)> = tx
            .query_row(
                "SELECT activation_epoch,sequence,digest FROM durable_tails WHERE actor_id=?1",
                params![actor],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(failure)?;

        if let Some((old_epoch, old_seq, ref old_digest)) = current {
            // Do not let a corrupt durable_tails row be laundered into a
            // successful new commit, including a same-digest retry.
            if old_epoch <= 0
                || old_seq <= 0
                || old_digest.len() != 64
                || !old_digest.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(invalid("malformed committed durable tail"));
            }
            let recorded: Option<(i64, String)> = tx
                .query_row(
                    "SELECT activation_epoch,digest FROM durable_transitions
                 WHERE actor_id=?1 AND sequence=?2",
                    params![actor, old_seq],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(failure)?;
            if recorded
                .as_ref()
                .is_none_or(|(record_epoch, record_digest)| {
                    *record_epoch != old_epoch || record_digest != old_digest
                })
            {
                return Err(invalid(
                    "committed durable tail conflicts with transition history",
                ));
            }
            if epoch == old_epoch && seq == old_seq {
                if old_digest == &digest_hex {
                    // Nothing to update; the old exact digest is committed.
                    return Ok(DurableCommit {
                        actor_id: transition.actor_id,
                        activation_epoch: transition.activation_epoch,
                        sequence: transition.sequence,
                        digest,
                    });
                }
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "conflicting durable digest at the same epoch/sequence",
                ));
            }
            if epoch < old_epoch {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "stale durable activation epoch",
                ));
            }
            if predecessor != old_seq {
                return Err(invalid(
                    "durable transition predecessor does not match committed tail",
                ));
            }
        } else {
            let legacy_tail: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(sequence),0) FROM (
                    SELECT sequence FROM snapshots WHERE actor_id=?1
                    UNION ALL SELECT sequence FROM journal WHERE actor_id=?1
                    UNION ALL SELECT sequence FROM workflow_events WHERE actor_id=?1
                    UNION ALL SELECT sequence FROM events WHERE actor_id=?1
                )",
                    params![actor],
                    |r| r.get(0),
                )
                .map_err(failure)?;
            if predecessor != legacy_tail {
                return Err(invalid(
                    "first atomic durable transition does not match legacy tail",
                ));
            }
        }

        if let (Some(s), Some((state, crdt, fields, authority))) =
            (&transition.snapshot, &snapshot_data)
        {
            tx.execute(
                "INSERT INTO snapshots (
                    actor_id,sequence,activation_epoch,state,waiting_signal,
                    crdt_snapshot,crdt_field_map,schema_name,authority_tokens
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(actor_id) DO UPDATE SET
                    sequence=excluded.sequence,
                    activation_epoch=excluded.activation_epoch,
                    state=excluded.state,
                    waiting_signal=excluded.waiting_signal,
                    crdt_snapshot=excluded.crdt_snapshot,
                    crdt_field_map=excluded.crdt_field_map,
                    schema_name=excluded.schema_name,
                    authority_tokens=excluded.authority_tokens",
                params![
                    actor,
                    seq,
                    epoch,
                    state,
                    s.waiting_signal,
                    crdt,
                    fields,
                    s.schema_name,
                    authority
                ],
            )
            .map_err(failure)?;
        }
        if let (Some(cmd), Some(payload)) = (&transition.command, &command_json) {
            tx.execute(
                "INSERT INTO journal (actor_id,sequence,behavior_id,payload)
                 VALUES (?1,?2,?3,?4)",
                params![
                    actor,
                    bounded(cmd.sequence)?,
                    i64::from(cmd.behavior_id),
                    payload
                ],
            )
            .map_err(failure)?;
        }
        for (i, event) in workflow_json.iter().enumerate() {
            tx.execute(
                "INSERT INTO durable_workflow_events(actor_id,sequence,ordinal,event)
                 VALUES (?1,?2,?3,?4)",
                params![actor, seq, i64::try_from(i).map_err(invalid)?, event],
            )
            .map_err(failure)?;
        }
        for (i, event) in domain_json.iter().enumerate() {
            tx.execute(
                "INSERT INTO durable_domain_events(actor_id,sequence,ordinal,event)
                 VALUES (?1,?2,?3,?4)",
                params![actor, seq, i64::try_from(i).map_err(invalid)?, event],
            )
            .map_err(failure)?;
        }
        for (i, record) in effect_json.iter().enumerate() {
            tx.execute(
                "INSERT INTO durable_effect_records(actor_id,sequence,ordinal,record)
                 VALUES (?1,?2,?3,?4)",
                params![actor, seq, i64::try_from(i).map_err(invalid)?, record],
            )
            .map_err(failure)?;
        }
        for (msg, payload) in transition.outbox.iter().zip(&outbox_json) {
            tx.execute(
                "INSERT INTO durable_outbox (
                    actor_id,sequence,ordinal,destination_actor_id,behavior_id,payload
                ) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    actor,
                    seq,
                    i64::from(msg.ordinal),
                    bounded(msg.destination_actor_id)?,
                    i64::from(msg.behavior_id),
                    payload
                ],
            )
            .map_err(failure)?;
        }
        tx.execute(
            "INSERT INTO durable_transitions (
                actor_id,sequence,activation_epoch,expected_previous_sequence,digest
            ) VALUES (?1,?2,?3,?4,?5)",
            params![actor, seq, epoch, predecessor, digest_hex],
        )
        .map_err(failure)?;
        if let Some((old_epoch, old_seq, old_digest)) = current {
            let changed = tx
                .execute(
                    "UPDATE durable_tails
                 SET activation_epoch=?1,sequence=?2,digest=?3
                 WHERE actor_id=?4 AND activation_epoch=?5
                   AND sequence=?6 AND digest=?7",
                    params![epoch, seq, digest_hex, actor, old_epoch, old_seq, old_digest],
                )
                .map_err(failure)?;
            if changed != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "durable tail compare-and-swap must affect exactly one row",
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO durable_tails(actor_id,activation_epoch,sequence,digest)
                 VALUES (?1,?2,?3,?4)",
                params![actor, epoch, seq, digest_hex],
            )
            .map_err(failure)?;
        }
        tx.commit().map_err(failure)?;
        Ok(DurableCommit {
            actor_id: transition.actor_id,
            activation_epoch: transition.activation_epoch,
            sequence: transition.sequence,
            digest,
        })
    }
}
