//! Synthetic competing writer, always in its own OS process.
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry,
    PersistedValue, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang_rusqlite_recovery_reader::RusqliteAtomicWriter;
use std::{io, path::PathBuf};
// Test-only deliberate SIGKILL window after staging every durable table in
// an uncommitted BEGIN IMMEDIATE transaction. This is never a production
// transaction writer; it verifies SQLite's cold-process crash recovery.
fn stage_then_block(path: &std::path::Path) -> io::Result<()> {
    use std::io::Write;
    let mut conn = rusqlite::Connection::open(path).map_err(io::Error::other)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(io::Error::other)?;
    tx.execute("UPDATE snapshots SET sequence=3 WHERE actor_id=42", [])
        .map_err(io::Error::other)?;
    tx.execute(
        "INSERT INTO journal(actor_id,sequence,behavior_id,payload) VALUES (42,3,7,'[]')",
        [],
    )
    .map_err(io::Error::other)?;
    tx.execute(
        "INSERT INTO durable_workflow_events(actor_id,sequence,ordinal,event) VALUES (42,3,0,'{}')",
        [],
    )
    .map_err(io::Error::other)?;
    tx.execute(
        "INSERT INTO durable_domain_events(actor_id,sequence,ordinal,event) VALUES (42,3,0,'{}')",
        [],
    )
    .map_err(io::Error::other)?;
    tx.execute(
        "INSERT INTO durable_effect_records(actor_id,sequence,ordinal,record) VALUES (42,3,0,'{}')",
        [],
    )
    .map_err(io::Error::other)?;
    tx.execute("INSERT INTO durable_outbox(actor_id,sequence,ordinal,destination_actor_id,behavior_id,payload) VALUES (42,3,0,89,9,'[]')",[])
        .map_err(io::Error::other)?;
    tx.execute("INSERT INTO durable_transitions(actor_id,sequence,activation_epoch,expected_previous_sequence,digest) VALUES (42,3,1,2,?1)",[&"f".repeat(64)])
        .map_err(io::Error::other)?;
    tx.execute(
        "UPDATE durable_tails SET sequence=3,digest=?1 WHERE actor_id=42",
        [&"f".repeat(64)],
    )
    .map_err(io::Error::other)?;
    println!("READY_UNCOMMITTED");
    io::stdout().flush()?;
    std::thread::sleep(std::time::Duration::from_secs(30));
    // No COMMIT. A normal exit also rolls back; the parent intentionally
    // kills this process before the sleep finishes.
    drop(tx);
    Ok(())
}

fn main() -> io::Result<()> {
    let a = std::env::args().collect::<Vec<_>>();
    if a.len() == 3 && a[1] == "--precommit-block" {
        return stage_then_block(std::path::Path::new(&a[2]));
    }
    if a.len() != 7 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected db actor epoch sequence predecessor tag",
        ));
    }
    let db = PathBuf::from(&a[1]);
    let actor = a[2]
        .parse::<u64>()
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    let epoch = a[3]
        .parse::<u64>()
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    let seq = a[4]
        .parse::<u64>()
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    let predecessor = a[5]
        .parse::<u64>()
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    let tag = &a[6];
    let transition = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: actor,
        activation_epoch: epoch,
        sequence: seq,
        expected_previous_sequence: predecessor,
        command: Some(JournalEntry {
            sequence: seq,
            behavior_id: 7,
            payload: vec![PersistedValue::String(tag.clone())],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: actor,
            sequence: seq,
            activation_epoch: epoch,
            state: [("tag".to_string(), PersistedValue::String(tag.clone()))].into(),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence: seq,
            replay_id: None,
            name: tag.clone(),
            args: vec![],
        }],
        domain_events: vec![EventEntry {
            sequence: seq,
            field_name: "tag".to_string(),
            event_name: "Changed".to_string(),
            args: vec![],
            value: PersistedValue::String(tag.clone()),
        }],
        durable_effects: vec![],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: 89,
            ordinal: 0,
            behavior_id: 9,
            payload: vec![PersistedValue::String(tag.clone())],
        }],
    };
    let mut writer = RusqliteAtomicWriter::open_existing(db)?;
    writer.commit(&transition)?;
    Ok(())
}
