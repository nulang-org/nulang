use nulang::runtime::{
    ActorSnapshot, DurableTransition, JournalEntry, LibsqlStore, PersistedValue, PersistenceStore,
    WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use std::{io, path::PathBuf};

fn commit(writer: &mut LibsqlStore, sequence: u64) -> io::Result<()> {
    writer.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 42,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence: sequence - 1,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 3,
            payload: vec![PersistedValue::Int(sequence as i64)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: 42,
            sequence,
            activation_epoch: 1,
            state: [("count".to_string(), PersistedValue::Int(sequence as i64))].into(),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence,
            replay_id: None,
            name: "written".to_string(),
            args: vec![],
        }],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    })?;
    Ok(())
}

fn main() -> io::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing fixture DB path"))?;
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "fixture path must not already exist",
        ));
    }
    let mut writer = LibsqlStore::new(&path)?;
    commit(&mut writer, 1)?;
    commit(&mut writer, 2)?;
    println!("PASS libSQL fixture created with two atomic durable commits");
    Ok(())
}
