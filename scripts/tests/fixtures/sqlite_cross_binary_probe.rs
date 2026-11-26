use nulang::runtime::{
    ActorSnapshot, JournalEntry, LibsqlStore, PersistedValue, PersistenceStore, WorkflowEvent,
};
use std::{env, path::Path};

const ACTOR: u64 = 17;
fn snapshot(sequence: u64, count: i64) -> ActorSnapshot {
    let mut s = ActorSnapshot {
        actor_id: ACTOR,
        sequence,
        activation_epoch: 3,
        ..Default::default()
    };
    s.state.insert("count".into(), PersistedValue::Int(count));
    s.state
        .insert("label".into(), PersistedValue::String("legacy: α".into()));
    s.state.insert("ready".into(), PersistedValue::Bool(true));
    s.state.insert("optional".into(), PersistedValue::Nil);
    s.waiting_signal = Some("approved".into());
    s
}
fn verify(store: &LibsqlStore, upgraded: bool) {
    let (sequence, count, latest, journals, workflow) = if upgraded {
        (5, 9, 6, 2, 2)
    } else {
        (3, 7, 4, 1, 1)
    };
    let actual = store.load_snapshot(ACTOR).expect("snapshot");
    let expected = snapshot(sequence, count);
    assert_eq!(actual.actor_id, ACTOR);
    assert_eq!(actual.sequence, sequence);
    assert_eq!(actual.activation_epoch, 3);
    assert_eq!(actual.state, expected.state);
    assert_eq!(actual.waiting_signal, expected.waiting_signal);
    assert_eq!(store.latest_sequence(ACTOR), latest);
    let js = store.read_journal(ACTOR);
    assert_eq!(js.len(), journals);
    assert_eq!(js[0].sequence, 4);
    assert_eq!(js[0].behavior_id, 2);
    assert_eq!(
        js[0].payload,
        vec![
            PersistedValue::String("legacy payload".into()),
            PersistedValue::Nil
        ]
    );
    if upgraded {
        assert_eq!(js[1].sequence, 6);
        assert_eq!(js[1].payload, vec![PersistedValue::Int(9)]);
    }
    let events = store.read_workflow_events(ACTOR);
    assert_eq!(events.len(), workflow);
    match &events[0] {
        WorkflowEvent::TimerSet {
            sequence,
            name,
            duration_ms,
            ..
        } => {
            assert_eq!(
                (*sequence, name.as_str(), *duration_ms),
                (2, "reminder", 1250)
            );
        }
        other => panic!("expected TimerSet, got {other:?}"),
    }
    if upgraded {
        match &events[1] {
            WorkflowEvent::TimerFired { sequence, name, .. } => {
                assert_eq!((*sequence, name.as_str()), (5, "reminder"))
            }
            other => panic!("expected TimerFired, got {other:?}"),
        }
    }
    println!("validated snapshot sequence={sequence}, activation_epoch=3, latest={latest}, journals={journals}, workflow_events={workflow}");
}
fn main() {
    let args: Vec<_> = env::args().collect();
    assert_eq!(args.len(), 3);
    let mut store = LibsqlStore::new(Path::new(&args[2])).expect("open disposable store");
    match args[1].as_str() {
        "seed" => {
            assert!(store.load_snapshot(ACTOR).is_none());
            store.save_snapshot(snapshot(3, 7)).unwrap();
            store
                .append_journal(
                    ACTOR,
                    JournalEntry {
                        sequence: 4,
                        behavior_id: 2,
                        payload: vec![
                            PersistedValue::String("legacy payload".into()),
                            PersistedValue::Nil,
                        ],
                    },
                )
                .unwrap();
            store
                .append_timer_set(ACTOR, 2, "reminder".into(), 1250)
                .unwrap();
            verify(&store, false);
        }
        "check-old" => verify(&store, false),
        "upgrade" | "upgrade-crash" => {
            verify(&store, false);
            store.save_snapshot(snapshot(5, 9)).unwrap();
            store
                .append_journal(
                    ACTOR,
                    JournalEntry {
                        sequence: 6,
                        behavior_id: 3,
                        payload: vec![PersistedValue::Int(9)],
                    },
                )
                .unwrap();
            store
                .append_timer_fired(ACTOR, 5, "reminder".into())
                .unwrap();
            verify(&store, true);
            if args[1] == "upgrade-crash" {
                // The committed state is verified, but the store is still
                // open. Only the parent test harness may terminate this
                // child, preventing normal Drop/last-connection checkpoint.
                use std::io::Write;
                println!("NULANG_WAL_CRASH_READY");
                std::io::stdout().flush().unwrap();
                loop {
                    std::thread::park();
                }
            }
        }
        "check-new" => verify(&store, true),
        _ => panic!("invalid probe mode"),
    }
}
