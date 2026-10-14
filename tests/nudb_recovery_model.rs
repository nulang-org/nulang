//! Deterministic single-tablet recovery model for NuDB.
//!
//! The reference history below is intentionally independent of NuDB's MVCC,
//! checkpoint and WAL internals. Fixed seeds make any failure reproducible.
//! This is NOT a replacement for subprocess SIGKILL tests, writer fencing,
//! concurrent commit testing, or a distributed consistency checker.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);
const KEYS: [&[u8]; 5] = [b"b0", b"g0", b"k0", b"p0", b"y0"];
const SEEDS: [u64; 4] = [
    0x0000_0000_0000_0001,
    0xdead_beef_cafe_babe,
    0x1234_5678_9abc_def0,
    0xfeed_face_1020_3040,
];

struct Fixture {
    wal: PathBuf,
}

impl Fixture {
    fn new(seed: u64) -> Self {
        let wal = std::env::temp_dir().join(format!(
            "nulang_nudb_model_{seed:016x}_{}_{}.wal",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        Self { wal }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.wal);
        let checkpoint = self.wal.with_extension("checkpoint");
        let _ = fs::remove_file(&checkpoint);
        let _ = fs::remove_file(format!("{}.tmp", checkpoint.display()));
        let _ = fs::remove_file(format!("{}.reclaim.tmp", self.wal.display()));
    }
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(93).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        9,
    )
    .unwrap()
}

/// Xorshift64: predictable operation schedules without an RNG dependency.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn assert_history(
    tablet: &WalBackedTablet,
    history: &[BTreeMap<Vec<u8>, Vec<u8>>],
    seed: u64,
    step: usize,
) {
    assert_eq!(
        tablet.current_sequence() as usize,
        history.len() - 1,
        "wrong durable tail seed={seed:#x} step={step}"
    );

    for (sequence, rows) in history.iter().enumerate() {
        for key in KEYS {
            assert_eq!(
                tablet.read_at(key, sequence as u64).unwrap(),
                rows.get(key).map(Vec::as_slice),
                "MVCC divergence seed={seed:#x} step={step} snapshot={sequence} key={key:?}"
            );
        }

        // A prepared-but-rejected mutation must never become visible.
        assert_eq!(
            tablet.read_at(b"x0", sequence as u64).unwrap(),
            None,
            "rejected mutation appeared seed={seed:#x} step={step} snapshot={sequence}"
        );
    }
}

#[test]
fn seeded_committed_history_matches_mvcc_before_and_after_recovery() {
    for seed in SEEDS {
        let fixture = Fixture::new(seed);
        let mut tablet = WalBackedTablet::open(descriptor(), &fixture.wal).unwrap();
        let mut rng = seed;

        // Index N is the independently modeled state at committed sequence N.
        let mut history = vec![BTreeMap::<Vec<u8>, Vec<u8>>::new()];
        assert_history(&tablet, &history, seed, 0);

        for step in 0..32 {
            let first_index = next(&mut rng) as usize % KEYS.len();
            let key = KEYS[first_index].to_vec();
            let delete = next(&mut rng) % 4 == 0;
            let first_mutation = if delete {
                TabletMutation::Delete { key: key.clone() }
            } else {
                TabletMutation::Put {
                    key: key.clone(),
                    value: format!("seed={seed:#x};step={step}").into_bytes(),
                }
            };
            let mut mutations = vec![first_mutation];

            // Every third transaction changes two DISTINCT keys atomically.
            // Repeated keys within a batch are tracked separately in #1702.
            if step % 3 == 0 {
                let second_index =
                    (first_index + 1 + next(&mut rng) as usize % (KEYS.len() - 1)) % KEYS.len();
                let other_key = KEYS[second_index].to_vec();
                let second_mutation = if next(&mut rng) % 3 == 0 {
                    TabletMutation::Delete { key: other_key }
                } else {
                    TabletMutation::Put {
                        key: other_key,
                        value: format!("seed={seed:#x};step={step};second").into_bytes(),
                    }
                };
                mutations.push(second_mutation);
            }

            let predecessor = tablet.current_sequence();

            // Prepare a valid write before the real commit, then submit it
            // afterward. It must fail without consuming a WAL sequence.
            let stale = tablet
                .prepare_write(
                    9,
                    predecessor,
                    vec![TabletMutation::Put {
                        key: b"x0".to_vec(),
                        value: b"must-not-commit".to_vec(),
                    }],
                )
                .unwrap();
            let prepared = tablet.prepare_write(9, predecessor, mutations.clone()).unwrap();
            let acknowledged = tablet.commit(prepared).unwrap();
            assert_eq!(acknowledged, predecessor + 1);
            assert_eq!(
                tablet.commit(stale).unwrap_err(),
                WalBackedError::Tablet(TabletError::SequenceMismatch {
                    committed: acknowledged,
                    expected_previous: predecessor,
                }),
                "stale write unexpectedly committed seed={seed:#x} step={step}"
            );

            let mut expected = history.last().unwrap().clone();
            for mutation in mutations {
                match mutation {
                    TabletMutation::Put { key, value } => {
                        expected.insert(key, value);
                    }
                    TabletMutation::Delete { key } => {
                        expected.remove(&key);
                    }
                }
            }
            history.push(expected);
            assert_history(&tablet, &history, seed, step);

            // Interleave checkpoint publication, WAL reclamation and reopen
            // cycles without changing the independent reference history.
            let operation = next(&mut rng) % 7;
            if operation == 0 {
                tablet.publish_checkpoint().unwrap();
            } else if operation == 1 {
                tablet.checkpoint().unwrap();
            }
            if operation <= 1 || step % 5 == 0 {
                drop(tablet);
                tablet = WalBackedTablet::open(descriptor(), &fixture.wal).unwrap();
                assert_history(&tablet, &history, seed, step);
            }
        }

        drop(tablet);
        let reopened = WalBackedTablet::open(descriptor(), &fixture.wal).unwrap();
        assert_history(&reopened, &history, seed, 32);
    }
}
