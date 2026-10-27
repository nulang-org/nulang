//! Test-only, opt-in local Turso durable-transition writer.
//!
//! This is deliberately NOT a production `PersistenceStore` implementation.
//! It operates only on an already initialized real Nulang schema, without
//! altering release features, cloud authentication, or database migration.
use nulang::runtime::{DurableCommit, DurableTransition};
use std::{fmt::Display, io};

fn sql_error(error: impl Display) -> io::Error {
    io::Error::other(format!("Turso local transaction: {error}"))
}

fn integer(value: u64) -> io::Result<i64> {
    i64::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable transition integer exceeds SQLite's signed 64-bit range",
        )
    })
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> io::Result<String> {
    serde_json::to_string(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

async fn inside_transaction(
    conn: &turso::Connection,
    t: &DurableTransition,
    digest: &str,
) -> io::Result<()> {
    let actor = integer(t.actor_id)?;
    let epoch = integer(t.activation_epoch)?;
    let seq = integer(t.sequence)?;
    let prev = integer(t.expected_previous_sequence)?;

    let tail = {
        let mut rows = conn
            .query(
                "SELECT activation_epoch, sequence, digest FROM durable_tails WHERE actor_id=?1",
                (actor,),
            )
            .await
            .map_err(sql_error)?;
        let tail = match rows.next().await.map_err(sql_error)? {
            Some(row) => Some((
                row.get::<i64>(0).map_err(sql_error)?,
                row.get::<i64>(1).map_err(sql_error)?,
                row.get::<String>(2).map_err(sql_error)?,
            )),
            None => None,
        };
        drop(rows);
        tail
    };

    if let Some((old_epoch, old_seq, old_digest)) = &tail {
        if epoch == *old_epoch && seq == *old_seq {
            if digest == old_digest {
                // Exactly-once result for the identical logical transition.
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conflicting durable transition digest at committed position",
            ));
        }
        if epoch < *old_epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "stale durable actor activation epoch",
            ));
        }
        if prev != *old_seq {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "durable predecessor does not match committed tail",
            ));
        }
    } else {
        // Match the original LibsqlStore migration boundary; do not reset an
        // actor's sequence if legacy history exists but has no atomic tail.
        let mut rows = conn
            .query(
                "SELECT COALESCE(MAX(sequence), 0) FROM (
                SELECT sequence FROM snapshots WHERE actor_id=?1
                UNION ALL SELECT sequence FROM journal WHERE actor_id=?1
                UNION ALL SELECT sequence FROM workflow_events WHERE actor_id=?1
                UNION ALL SELECT sequence FROM events WHERE actor_id=?1
             )",
                (actor,),
            )
            .await
            .map_err(sql_error)?;
        let legacy = rows
            .next()
            .await
            .map_err(sql_error)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing legacy tail result")
            })?
            .get::<i64>(0)
            .map_err(sql_error)?;
        drop(rows);
        if prev != legacy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "durable predecessor does not match legacy history",
            ));
        }
    }

    if let Some(snapshot) = &t.snapshot {
        let state = json(&snapshot.state)?;
        let crdt = json(&snapshot.crdt_snapshot)?;
        let fields = json(&snapshot.crdt_field_map)?;
        let tokens = json(&snapshot.authority_tokens)?;
        conn.execute(
            "INSERT INTO snapshots
                (actor_id,sequence,activation_epoch,state,waiting_signal,crdt_snapshot,crdt_field_map,schema_name,authority_tokens)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(actor_id) DO UPDATE SET
                sequence=excluded.sequence,
                activation_epoch=excluded.activation_epoch,
                state=excluded.state,
                waiting_signal=excluded.waiting_signal,
                crdt_snapshot=excluded.crdt_snapshot,
                crdt_field_map=excluded.crdt_field_map,
                schema_name=excluded.schema_name,
                authority_tokens=excluded.authority_tokens",
            turso::params![
                actor, seq, epoch, state.as_str(), snapshot.waiting_signal.as_deref(),
                crdt.as_str(), fields.as_str(), snapshot.schema_name.as_deref(), tokens.as_str()
            ],
        ).await.map_err(sql_error)?;
    }

    if let Some(command) = &t.command {
        let payload = json(&command.payload)?;
        conn.execute(
            "INSERT INTO journal (actor_id,sequence,behavior_id,payload) VALUES (?1,?2,?3,?4)",
            (actor, seq, i64::from(command.behavior_id), payload.as_str()),
        )
        .await
        .map_err(sql_error)?;
    }

    for (ordinal, event) in t.workflow_events.iter().enumerate() {
        let record = json(event)?;
        conn.execute(
            "INSERT INTO durable_workflow_events (actor_id,sequence,ordinal,event) VALUES (?1,?2,?3,?4)",
            (actor, seq, integer(ordinal as u64)?, record.as_str()),
        ).await.map_err(sql_error)?;
    }
    for (ordinal, event) in t.domain_events.iter().enumerate() {
        let record = json(event)?;
        conn.execute(
            "INSERT INTO durable_domain_events (actor_id,sequence,ordinal,event) VALUES (?1,?2,?3,?4)",
            (actor, seq, integer(ordinal as u64)?, record.as_str()),
        ).await.map_err(sql_error)?;
    }
    for (ordinal, effect) in t.durable_effects.iter().enumerate() {
        let record = String::from_utf8(
            effect
                .to_json()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        conn.execute(
            "INSERT INTO durable_effect_records (actor_id,sequence,ordinal,record) VALUES (?1,?2,?3,?4)",
            (actor, seq, integer(ordinal as u64)?, record.as_str()),
        ).await.map_err(sql_error)?;
    }
    for message in &t.outbox {
        let payload = json(&message.payload)?;
        conn.execute(
            "INSERT INTO durable_outbox
               (actor_id,sequence,ordinal,destination_actor_id,behavior_id,payload)
             VALUES (?1,?2,?3,?4,?5,?6)",
            turso::params![
                actor,
                seq,
                i64::from(message.ordinal),
                integer(message.destination_actor_id)?,
                i64::from(message.behavior_id),
                payload.as_str()
            ],
        )
        .await
        .map_err(sql_error)?;
    }

    conn.execute(
        "INSERT INTO durable_transitions
             (actor_id,sequence,activation_epoch,expected_previous_sequence,digest)
         VALUES (?1,?2,?3,?4,?5)",
        (actor, seq, epoch, prev, digest),
    )
    .await
    .map_err(sql_error)?;

    if let Some((old_epoch, old_seq, old_digest)) = tail {
        let changed = conn
            .execute(
                "UPDATE durable_tails SET activation_epoch=?1, sequence=?2, digest=?3
             WHERE actor_id=?4 AND activation_epoch=?5 AND sequence=?6 AND digest=?7",
                turso::params![
                    epoch,
                    seq,
                    digest,
                    actor,
                    old_epoch,
                    old_seq,
                    old_digest.as_str()
                ],
            )
            .await
            .map_err(sql_error)?;
        if changed != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "durable tail CAS changed before commit",
            ));
        }
    } else {
        conn.execute(
            "INSERT INTO durable_tails (actor_id,activation_epoch,sequence,digest)
             VALUES (?1,?2,?3,?4)",
            (actor, epoch, seq, digest),
        )
        .await
        .map_err(sql_error)?;
    }
    Ok(())
}

/// Attempt a single all-or-nothing local Turso write, using Nulang's exact
/// canonical transition digest and schema. Caller must opt in explicitly.
pub async fn commit(conn: &turso::Connection, t: &DurableTransition) -> io::Result<DurableCommit> {
    // FULL is a per-connection SQLite policy. Fail closed before BEGIN if
    // a caller inadvertently used NORMAL/OFF (no fsync on each commit).
    let mut sync = conn
        .query("PRAGMA synchronous", ())
        .await
        .map_err(sql_error)?;
    let mode = sync
        .next()
        .await
        .map_err(sql_error)?
        .ok_or_else(|| io::Error::other("Turso has no synchronous PRAGMA result"))?
        .get::<i64>(0)
        .map_err(sql_error)?;
    drop(sync);
    if mode != 2 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing durable Turso commit without synchronous=FULL",
        ));
    }
    let digest = t.digest()?; // Also enforces the original structural contract.
                              // Reject unsafe unsigned-to-signed casts BEFORE BEGIN, fail-closed.
    for n in [
        t.actor_id,
        t.sequence,
        t.activation_epoch,
        t.expected_previous_sequence,
    ] {
        integer(n)?;
    }
    let digest_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    conn.execute("BEGIN IMMEDIATE", ())
        .await
        .map_err(sql_error)?;
    if let Err(error) = inside_transaction(conn, t, &digest_hex).await {
        conn.execute("ROLLBACK", ()).await.map_err(|rollback| {
            io::Error::other(format!("{error}; rollback also failed: {rollback}"))
        })?;
        return Err(error);
    }
    if let Err(error) = conn.execute("COMMIT", ()).await {
        let rollback = conn.execute("ROLLBACK", ()).await;
        return Err(io::Error::other(format!(
            "Turso COMMIT did not complete ({error}); rollback: {rollback:?}; commit status is unverified"
        )));
    }
    Ok(DurableCommit {
        actor_id: t.actor_id,
        activation_epoch: t.activation_epoch,
        sequence: t.sequence,
        digest,
    })
}
