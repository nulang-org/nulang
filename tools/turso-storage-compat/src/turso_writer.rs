//! Test-only Turso-backed atomic transition prototype.
//!
//! This is NOT installed as Nulang's production PersistenceStore. It supports
//! snapshots, commands, replayable workflow/domain events, versioned durable
//! effects, and durable outbox records inside one atomic SQLite transaction.
use nulang::runtime::{DurableCommit, DurableTransition};
use std::io;

fn db_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn sql_int(value: u64) -> io::Result<i64> {
    i64::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable identifier exceeds SQLite signed integer range",
        )
    })
}

fn json<T: serde::Serialize>(value: &T) -> io::Result<String> {
    serde_json::to_string(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

pub(crate) async fn commit_transition(
    conn: &turso::Connection,
    transition: DurableTransition,
) -> io::Result<DurableCommit> {
    // This public NuLang method performs structural checks and computes the
    // same canonical BLAKE3 digest that the production libSQL writer stores.
    let digest = transition.digest()?;
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let actor = sql_int(transition.actor_id)?;
    let epoch = sql_int(transition.activation_epoch)?;
    let sequence = sql_int(transition.sequence)?;
    let previous = sql_int(transition.expected_previous_sequence)?;

    // Serialize and range-check all data before BEGIN IMMEDIATE. There are
    // no external calls or user code inside the durable SQL transaction.
    let snapshot = transition
        .snapshot
        .as_ref()
        .map(|s| -> io::Result<_> {
            Ok((
                json(&s.state)?,
                json(&s.crdt_snapshot)?,
                json(&s.crdt_field_map)?,
                s.schema_name.clone(),
                json(&s.authority_tokens)?,
                s.waiting_signal.clone(),
            ))
        })
        .transpose()?;
    let command = transition
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
            let bytes = record
                .to_json()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            String::from_utf8(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let outbox = transition
        .outbox
        .iter()
        .map(|m| {
            Ok::<_, io::Error>((
                i64::from(m.ordinal),
                sql_int(m.destination_actor_id)?,
                i64::from(m.behavior_id),
                json(&m.payload)?,
            ))
        })
        .collect::<io::Result<Vec<_>>>()?;

    let receipt = DurableCommit {
        actor_id: transition.actor_id,
        activation_epoch: transition.activation_epoch,
        sequence: transition.sequence,
        digest,
    };

    // BEGIN IMMEDIATE is the SQLite single-writer serialization boundary;
    // all fencing, legacy-tail reads and writes happen under this lock.
    conn.execute("BEGIN IMMEDIATE", ())
        .await
        .map_err(db_error)?;
    let result: io::Result<()> = async {
        let tail = {
            let mut rows = conn
                .query(
                    "SELECT activation_epoch, sequence, digest
                     FROM durable_tails WHERE actor_id=?1",
                    [actor],
                )
                .await
                .map_err(db_error)?;
            let value = match rows.next().await.map_err(db_error)? {
                Some(row) => {
                    let old_epoch: i64 = row.get(0).map_err(db_error)?;
                    let old_sequence: i64 = row.get(1).map_err(db_error)?;
                    let old_digest: String = row.get(2).map_err(db_error)?;
                    Some((old_epoch, old_sequence, old_digest))
                }
                None => None,
            };
            value
        };
        if let Some((old_epoch, old_sequence, old_digest)) = &tail {
            if epoch == *old_epoch && sequence == *old_sequence {
                if old_digest == &digest_hex {
                    return Ok(()); // exactly the same transition: no duplicate writes
                }
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "different digest already committed at epoch/sequence",
                ));
            }
            if epoch < *old_epoch {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "stale activation epoch",
                ));
            }
            if previous != *old_sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transition predecessor differs from durable tail",
                ));
            }
        } else {
            // Preserve the existing libSQL contract for the FIRST atomic
            // transition following any legacy snapshots, journal, or events.
            let mut rows = conn
                .query(
                    "SELECT COALESCE(MAX(sequence),0) FROM (
                        SELECT sequence FROM snapshots WHERE actor_id=?1
                        UNION ALL SELECT sequence FROM journal WHERE actor_id=?1
                        UNION ALL SELECT sequence FROM workflow_events WHERE actor_id=?1
                        UNION ALL SELECT sequence FROM events WHERE actor_id=?1
                    )",
                    [actor],
                )
                .await
                .map_err(db_error)?;
            let row = rows.next().await.map_err(db_error)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing legacy tail count")
            })?;
            let legacy_sequence: i64 = row.get(0).map_err(db_error)?;
            if previous != legacy_sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "first atomic commit does not extend persisted legacy sequence",
                ));
            }
        }

        if let (Some(s), Some((state, crdt, fields, schema, tokens, waiting))) =
            (&transition.snapshot, &snapshot)
        {
            conn.execute(
                "INSERT INTO snapshots
                 (actor_id, sequence, activation_epoch, state, waiting_signal, crdt_snapshot, crdt_field_map, schema_name, authority_tokens)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(actor_id) DO UPDATE SET
                    sequence=excluded.sequence, activation_epoch=excluded.activation_epoch,
                    state=excluded.state, waiting_signal=excluded.waiting_signal,
                    crdt_snapshot=excluded.crdt_snapshot, crdt_field_map=excluded.crdt_field_map,
                    schema_name=excluded.schema_name, authority_tokens=excluded.authority_tokens",
                turso::params![
                    actor, sql_int(s.sequence)?, epoch, state.as_str(),
                    waiting.as_deref(), crdt.as_str(), fields.as_str(),
                    schema.as_deref(), tokens.as_str()
                ],
            )
            .await
            .map_err(db_error)?;
        }
        if let (Some(c), Some(payload)) = (&transition.command, &command) {
            conn.execute(
                "INSERT INTO journal (actor_id, sequence, behavior_id, payload)
                 VALUES (?1,?2,?3,?4)",
                turso::params![actor, sequence, i64::from(c.behavior_id), payload.as_str()],
            )
            .await
            .map_err(db_error)?;
        }
        for (ordinal, event) in workflow_json.iter().enumerate() {
            conn.execute(
                "INSERT INTO durable_workflow_events
                 (actor_id, sequence, ordinal, event) VALUES (?1,?2,?3,?4)",
                turso::params![actor, sequence, sql_int(ordinal as u64)?, event.as_str()],
            )
            .await
            .map_err(db_error)?;
        }
        for (ordinal, event) in domain_json.iter().enumerate() {
            conn.execute(
                "INSERT INTO durable_domain_events
                 (actor_id, sequence, ordinal, event) VALUES (?1,?2,?3,?4)",
                turso::params![actor, sequence, sql_int(ordinal as u64)?, event.as_str()],
            )
            .await
            .map_err(db_error)?;
        }
        for (ordinal, record) in effect_json.iter().enumerate() {
            conn.execute(
                "INSERT INTO durable_effect_records
                 (actor_id, sequence, ordinal, record) VALUES (?1,?2,?3,?4)",
                turso::params![actor, sequence, sql_int(ordinal as u64)?, record.as_str()],
            )
            .await
            .map_err(db_error)?;
        }
        for (ordinal, destination, behavior, payload) in &outbox {
            conn.execute(
                "INSERT INTO durable_outbox
                 (actor_id, sequence, ordinal, destination_actor_id, behavior_id, payload)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                turso::params![
                    actor, sequence, *ordinal, *destination, *behavior, payload.as_str()
                ],
            )
            .await
            .map_err(db_error)?;
        }

        conn.execute(
            "INSERT INTO durable_transitions
             (actor_id, sequence, activation_epoch, expected_previous_sequence, digest)
             VALUES (?1,?2,?3,?4,?5)",
            turso::params![actor, sequence, epoch, previous, digest_hex.as_str()],
        )
        .await
        .map_err(db_error)?;

        if let Some((old_epoch, old_sequence, old_digest)) = tail {
            let affected = conn
                .execute(
                    "UPDATE durable_tails
                     SET activation_epoch=?1, sequence=?2, digest=?3
                     WHERE actor_id=?4 AND activation_epoch=?5 AND sequence=?6 AND digest=?7",
                    turso::params![
                        epoch, sequence, digest_hex.as_str(), actor,
                        old_epoch, old_sequence, old_digest.as_str()
                    ],
                )
                .await
                .map_err(db_error)?;
            if affected != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "durable tail changed during compare-and-swap",
                ));
            }
        } else {
            conn.execute(
                "INSERT INTO durable_tails (actor_id, activation_epoch, sequence, digest)
                 VALUES (?1,?2,?3,?4)",
                turso::params![actor, epoch, sequence, digest_hex.as_str()],
            )
            .await
            .map_err(db_error)?;
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            if let Err(error) = conn.execute("COMMIT", ()).await {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(db_error(error));
            }
            Ok(receipt)
        }
        Err(error) => {
            // Roll back all provisional snapshot/command/outbox/receipt rows.
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(error)
        }
    }
}
