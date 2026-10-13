use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry,
    NuDbTransitionJournal, PersistedValue, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_atomic_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn cleanup(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}

fn transition(actor_id: u64, sequence: u64, epoch: u64) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: epoch,
        sequence,
        expected_previous_sequence: sequence - 1,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 7,
            payload: vec![PersistedValue::String("accepted".to_string())],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id,
            sequence,
            activation_epoch: epoch,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::TimerSet {
            sequence,
            replay_id: None,
            name: "retry".to_string(),
            duration_ms: 250,
        }],
        domain_events: vec![EventEntry {
            sequence,
            field_name: "status".to_string(),
            event_name: "Updated".to_string(),
            args: vec![],
            value: PersistedValue::String("accepted".to_string()),
        }],
        durable_effects: vec![],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: actor_id + 1,
            ordinal: 0,
            behavior_id: 9,
            payload: vec![PersistedValue::Int(42)],
        }],
    }
}

#[test]
fn full_transition_round_trips_after_checkpoint_and_restart() {
    let path = temp_wal("roundtrip");
    cleanup(&path);
    let original = {
        let mut record = transition(42, 1, 2);
        let site = effect_site_id(
            "nudb-atomic-journal",
            EffectSiteOwnerKind::Behavior,
            "Order.run",
            "Provider.ask",
            0,
        );
        let spec = DurableEffectSpec::new(
            DurableEffectId::derive_from_site(42, "turn:1", site, 0),
            "Provider.ask",
            EffectBoundary::External,
            DeliverySemantics::AtLeastOnce,
        );
        record
            .durable_effects
            .push(DurableEffectPersistenceRecord::from_effect(
                DurableEffectRecord::prepare(spec, b"prompt"),
            ));
        record
    };
    let digest = original.digest().unwrap();

    {
        let mut journal = NuDbTransitionJournal::open(&path).unwrap();
        let committed = journal.commit_transition(original).unwrap();
        assert_eq!(committed.digest, digest);
        assert_eq!(journal.tablet_sequence(), 1);
        journal.checkpoint().unwrap();
    }

    let journal = NuDbTransitionJournal::open(&path).unwrap();
    let restored = journal.load_transition(42, 1).unwrap().unwrap();
    assert_eq!(restored.digest().unwrap(), digest);
    assert_eq!(restored.snapshot.unwrap().activation_epoch, 2);
    assert_eq!(restored.workflow_events.len(), 1);
    assert_eq!(restored.domain_events.len(), 1);
    assert_eq!(restored.durable_effects.len(), 1);
    assert_eq!(restored.outbox.len(), 1);
    let tail = journal.load_tail_position(42).unwrap().unwrap();
    assert_eq!(tail.activation_epoch, 2);
    assert_eq!(tail.sequence, 1);
    drop(journal);
    cleanup(&path);
}

#[test]
fn retries_are_idempotent_and_conflicts_or_stale_epochs_do_not_advance_wal() {
    let path = temp_wal("fencing");
    cleanup(&path);

    {
        let mut journal = NuDbTransitionJournal::open(&path).unwrap();
        let first = transition(99, 1, 4);
        let acknowledged = journal.commit_transition(first.clone()).unwrap();
        assert_eq!(journal.commit_transition(first).unwrap(), acknowledged);
        assert_eq!(journal.tablet_sequence(), 1);

        let mut conflict = transition(99, 1, 4);
        conflict.outbox[0].behavior_id = 10;
        assert_eq!(
            journal.commit_transition(conflict).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            journal
                .commit_transition(transition(99, 2, 3))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            journal
                .commit_transition(transition(99, 3, 4))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(journal.tablet_sequence(), 1);

        let next = transition(99, 2, 5);
        assert_eq!(journal.commit_transition(next).unwrap().sequence, 2);
        assert_eq!(journal.tablet_sequence(), 2);
        journal.checkpoint().unwrap();
    }

    let journal = NuDbTransitionJournal::open(&path).unwrap();
    assert_eq!(journal.load_tail_position(99).unwrap().unwrap().sequence, 2);
    assert_eq!(
        journal
            .load_transition(99, 1)
            .unwrap()
            .unwrap()
            .activation_epoch,
        4
    );
    assert_eq!(
        journal
            .load_transition(99, 2)
            .unwrap()
            .unwrap()
            .activation_epoch,
        5
    );
    drop(journal);
    cleanup(&path);
}

#[test]
fn actor_sequences_are_independent_within_one_tablet() {
    let path = temp_wal("multi_actor");
    cleanup(&path);
    let mut journal = NuDbTransitionJournal::open(&path).unwrap();

    journal.commit_transition(transition(11, 1, 1)).unwrap();
    journal.commit_transition(transition(22, 1, 2)).unwrap();
    journal.commit_transition(transition(11, 2, 1)).unwrap();
    assert_eq!(journal.tablet_sequence(), 3);
    assert_eq!(journal.load_tail_position(11).unwrap().unwrap().sequence, 2);
    assert_eq!(journal.load_tail_position(22).unwrap().unwrap().sequence, 1);
    assert!(journal.load_transition(22, 2).unwrap().is_none());

    drop(journal);
    cleanup(&path);
}

#[test]
fn invalid_transition_is_rejected_without_publishing_partial_state() {
    let path = temp_wal("invalid");
    cleanup(&path);
    let mut journal = NuDbTransitionJournal::open(&path).unwrap();
    let mut invalid = transition(8, 1, 1);
    invalid.outbox.push(invalid.outbox[0].clone());

    assert_eq!(
        journal.commit_transition(invalid).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(journal.tablet_sequence(), 0);
    assert!(journal.load_tail_position(8).unwrap().is_none());
    assert!(journal.load_transition(8, 1).unwrap().is_none());
    drop(journal);
    cleanup(&path);
}

fn mutate_raw_tablet(path: &PathBuf, mutation: TabletMutation) {
    let descriptor = TabletDescriptor::new(
        TabletId::new(1).unwrap(),
        KeyRange::new(Vec::new(), None).unwrap(),
        1,
    )
    .unwrap();
    let mut tablet = WalBackedTablet::open(descriptor, path).unwrap();
    let write = tablet
        .prepare_write(1, tablet.current_sequence(), vec![mutation])
        .unwrap();
    tablet.commit(write).unwrap();
}

fn raw_transition_key(actor_id: u64, sequence: u64) -> Vec<u8> {
    let mut key = vec![b'T'];
    key.extend_from_slice(&actor_id.to_be_bytes());
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

fn raw_tail_key(actor_id: u64) -> Vec<u8> {
    let mut key = vec![b'L'];
    key.extend_from_slice(&actor_id.to_be_bytes());
    key
}

#[test]
fn missing_tail_transition_is_not_reported_as_committed_on_retry() {
    let path = temp_wal("missing_last_transition");
    cleanup(&path);
    let original = transition(42, 1, 2);
    {
        let mut journal = NuDbTransitionJournal::open(&path).unwrap();
        journal.commit_transition(original.clone()).unwrap();
    }
    mutate_raw_tablet(
        &path,
        TabletMutation::Delete {
            key: raw_transition_key(42, 1),
        },
    );

    let mut journal = NuDbTransitionJournal::open(&path).unwrap();
    assert_eq!(
        journal.load_tail_position(42).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        journal.commit_transition(original).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(journal.tablet_sequence(), 2);
    drop(journal);
    cleanup(&path);
}

#[test]
fn forged_actor_tail_cannot_override_recovered_transition_digest() {
    let path = temp_wal("forged_tail");
    cleanup(&path);
    {
        let mut journal = NuDbTransitionJournal::open(&path).unwrap();
        journal.commit_transition(transition(55, 1, 1)).unwrap();
    }
    let forged_tail = serde_json::to_vec(&serde_json::json!({
        "activation_epoch": 1,
        "sequence": 1,
        "digest": vec![0u8; 32],
    }))
    .unwrap();
    mutate_raw_tablet(
        &path,
        TabletMutation::Put {
            key: raw_tail_key(55),
            value: forged_tail,
        },
    );

    let journal = NuDbTransitionJournal::open(&path).unwrap();
    assert_eq!(
        journal.load_tail_position(55).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    drop(journal);
    cleanup(&path);
}

#[test]
fn deleted_actor_tail_does_not_allow_resetting_existing_history() {
    let path = temp_wal("missing_tail");
    cleanup(&path);
    {
        let mut journal = NuDbTransitionJournal::open(&path).unwrap();
        journal.commit_transition(transition(77, 1, 1)).unwrap();
    }
    mutate_raw_tablet(
        &path,
        TabletMutation::Delete {
            key: raw_tail_key(77),
        },
    );

    let mut journal = NuDbTransitionJournal::open(&path).unwrap();
    assert_eq!(
        journal.load_tail_position(77).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        journal.commit_transition(transition(77, 1, 1)).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(journal.tablet_sequence(), 2);
    drop(journal);
    cleanup(&path);
}
