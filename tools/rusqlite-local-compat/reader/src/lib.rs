//! Isolated rusqlite compatibility and experimental atomic-transaction proofs.
//!
//! This independent test-tool crate is intentionally NOT a Nulang
//! `PersistenceStore`. Its read-only recovery reader returns Results and
//! never mutates persisted data; the separate experimental atomic writer can
//! write only an existing, current-schema SQLite database for verification.
//! Neither is linked into the production runtime, and the production
//! `LibsqlStore` remains the selected local durable backend until a fully
//! certified replacement and crash-recovery matrix exist.
mod atomic_writer;
pub use atomic_writer::RusqliteAtomicWriter;

use std::{
    collections::{BTreeSet, HashMap},
    io,
    path::Path,
    sync::Mutex,
};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use nulang::runtime::{
    ActorSnapshot, DurableTailPosition, JournalEntry, PersistedValue, WorkflowEvent,
};

fn sqlite(error: rusqlite::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn parsed<T: serde::de::DeserializeOwned>(s: &str) -> io::Result<T> {
    serde_json::from_str(s).map_err(invalid)
}

fn positive(value: i64, label: &str) -> io::Result<u64> {
    let out = u64::try_from(value).map_err(|_| invalid(format!("{label} is negative")))?;
    if out == 0 {
        return Err(invalid(format!("{label} must be non-zero")));
    }
    Ok(out)
}

fn nonnegative(value: i64, label: &str) -> io::Result<u64> {
    u64::try_from(value).map_err(|_| invalid(format!("{label} is negative")))
}

/// An independent, fail-closed reader of local SQLite databases created by
/// LibsqlStore. Does not implement PersistenceStore or modify schema/data.
#[derive(Debug)]
pub struct RusqliteRecoveryReader {
    conn: Mutex<Connection>,
}

impl RusqliteRecoveryReader {
    /// Open an existing, current-schema local durable database read-only.
    ///
    /// Missing or older schema elements cause errors rather than migration or
    /// defaulting missing durable history to an empty event stream.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "durable SQLite file is missing",
            ));
        }
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sqlite)?;

        let status: String = conn
            .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
            .map_err(sqlite)?;
        if status != "ok" {
            return Err(invalid("SQLite integrity check failed"));
        }

        // PRAGMA is not a migration; merely restricts the handle even if
        // future code accidentally attempts INSERT/DDL via this connection.
        conn.pragma_update(None, "query_only", "ON")
            .map_err(sqlite)?;

        for sql in [
            "SELECT actor_id,sequence,activation_epoch,state,waiting_signal,crdt_snapshot,crdt_field_map,schema_name,authority_tokens FROM snapshots LIMIT 0",
            "SELECT actor_id,sequence,behavior_id,payload FROM journal LIMIT 0",
            "SELECT actor_id,sequence,event FROM workflow_events LIMIT 0",
            "SELECT actor_id,sequence,activation_epoch,digest FROM durable_tails LIMIT 0",
            "SELECT actor_id,sequence,digest FROM durable_transitions LIMIT 0",
            "SELECT actor_id,sequence,ordinal,event FROM durable_workflow_events LIMIT 0",
            "SELECT actor_id,sequence,ordinal,event FROM durable_domain_events LIMIT 0",
            "SELECT actor_id,sequence,ordinal,record FROM durable_effect_records LIMIT 0",
            "SELECT actor_id,sequence,ordinal,destination_actor_id FROM durable_outbox LIMIT 0",
        ] {
            conn.prepare(sql).map_err(sqlite)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> io::Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| io::Error::other("SQLite recovery reader mutex poisoned"))
    }

    /// Read current snapshot. Invalid JSON, negative sequence, missing schema
    /// or invalid authority metadata are errors, not silent empty snapshots.
    pub fn snapshot(&self, actor_id: u64) -> io::Result<Option<ActorSnapshot>> {
        let actor = i64::try_from(actor_id).map_err(invalid)?;
        let conn = self.conn()?;
        let row = conn.query_row(
            "SELECT sequence,activation_epoch,state,waiting_signal,crdt_snapshot,crdt_field_map,schema_name,authority_tokens FROM snapshots WHERE actor_id=?1",
            params![actor], |row| {
                Ok((
                    row.get::<_,i64>(0)?, row.get::<_,i64>(1)?,
                    row.get::<_,String>(2)?, row.get::<_,Option<String>>(3)?,
                    row.get::<_,Option<String>>(4)?, row.get::<_,Option<String>>(5)?,
                    row.get::<_,Option<String>>(6)?, row.get::<_,Option<String>>(7)?,
                ))
            },
        ).optional().map_err(sqlite)?;
        let Some((sequence, epoch, state, waiting_signal, crdt, fields, schema, authority)) = row
        else {
            return Ok(None);
        };
        Ok(Some(ActorSnapshot {
            actor_id,
            sequence: nonnegative(sequence, "snapshot sequence")?,
            activation_epoch: positive(epoch, "snapshot activation epoch")?,
            state: parsed::<HashMap<String, PersistedValue>>(&state)?,
            waiting_signal,
            crdt_snapshot: match crdt {
                Some(v) => parsed(&v)?,
                None => None,
            },
            crdt_field_map: match fields {
                Some(v) => parsed(&v)?,
                None => None,
            },
            schema_name: schema,
            authority_tokens: match authority {
                Some(v) => parsed::<BTreeSet<String>>(&v)?,
                None => BTreeSet::new(), // legacy snapshots have no authority.
            },
        }))
    }

    /// Read the atomic tail; unlike a legacy history-only database, missing
    /// durable-tails table is an error detected when opening the reader.
    pub fn tail(&self, actor_id: u64) -> io::Result<Option<DurableTailPosition>> {
        let actor = i64::try_from(actor_id).map_err(invalid)?;
        let conn = self.conn()?;
        let result: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT activation_epoch,sequence,digest FROM durable_tails WHERE actor_id=?1",
                params![actor],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sqlite)?;
        result
            .map(|(epoch, sequence, digest)| {
                if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(invalid("malformed durable tail digest"));
                }
                Ok(DurableTailPosition {
                    activation_epoch: positive(epoch, "durable tail activation epoch")?,
                    sequence: positive(sequence, "durable tail sequence")?,
                })
            })
            .transpose()
    }

    /// Read all journal entries, never skipping malformed payloads.
    pub fn journal(&self, actor_id: u64) -> io::Result<Vec<JournalEntry>> {
        let actor = i64::try_from(actor_id).map_err(invalid)?;
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT sequence,behavior_id,payload FROM journal WHERE actor_id=?1 ORDER BY sequence"
        ).map_err(sqlite)?;
        let mut rows = statement.query(params![actor]).map_err(sqlite)?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite)? {
            let seq: i64 = row.get(0).map_err(sqlite)?;
            let behavior: i64 = row.get(1).map_err(sqlite)?;
            let payload: String = row.get(2).map_err(sqlite)?;
            entries.push(JournalEntry {
                sequence: positive(seq, "journal sequence")?,
                behavior_id: u16::try_from(behavior).map_err(invalid)?,
                payload: parsed::<Vec<PersistedValue>>(&payload)?,
            });
        }
        Ok(entries)
    }

    /// Combine legacy workflow_events with atomically committed rows in the
    /// same order as the existing LibsqlStore recovery reader.
    pub fn workflow_events(&self, actor_id: u64) -> io::Result<Vec<WorkflowEvent>> {
        let actor = i64::try_from(actor_id).map_err(invalid)?;
        let conn = self.conn()?;
        let mut events = Vec::new();
        for sql in [
            "SELECT event FROM workflow_events WHERE actor_id=?1 ORDER BY sequence",
            "SELECT event FROM durable_workflow_events WHERE actor_id=?1 ORDER BY sequence,ordinal",
        ] {
            let mut stmt = conn.prepare(sql).map_err(sqlite)?;
            let mut rows = stmt.query(params![actor]).map_err(sqlite)?;
            while let Some(row) = rows.next().map_err(sqlite)? {
                let encoded: String = row.get(0).map_err(sqlite)?;
                events.push(parsed::<WorkflowEvent>(&encoded)?);
            }
        }
        events.sort_by_key(WorkflowEvent::sequence);
        Ok(events)
    }
}
