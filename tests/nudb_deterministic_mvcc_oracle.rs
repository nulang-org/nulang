//! Reproducible, model-based NuDB single-node MVCC regression coverage.
//!
//! The reference model is an ordered map with one complete view per commit.
//! Each seed exercises repeated-key batches, tombstones, historical point
//! reads, bounded scans, checkpoint/WAL recovery, and split preparation.
//! This is NOT a distributed-transaction or simulated power-loss test.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation, TabletScanRow,
};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

const KEYS: [&[u8]; 6] = [b"a", b"b", b"g", b"m", b"t", b"y"];
const ROUNDS: u64 = 32;

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(71).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        5,
    )
    .unwrap()
}

fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn expected_rows(
    reference: &BTreeMap<Vec<u8>, Vec<u8>>,
    start: &[u8],
    end: &[u8],
    limit: usize,
) -> Vec<TabletScanRow> {
    reference
        .iter()
        .filter(|(key, _)| key.as_slice() >= start && key.as_slice() < end)
        .take(limit)
        .map(|(key, value)| TabletScanRow {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

fn temporary_wal(seed: u64) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_oracle_{}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed),
        seed
    ))
}

fn cleanup(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}

#[test]
fn deterministic_mvcc_reference_model_survives_checkpoint_recovery_and_split() {
    for initial_seed in [1_u64, 7, 0x1234_5678_9abc_def0, 0xdec0_ded1] {
        let path = temporary_wal(initial_seed);
        cleanup(&path);

        let mut state = initial_seed;
        let mut memory = MemoryTablet::new(descriptor());
        let mut durable = WalBackedTablet::open(descriptor(), &path).unwrap();
        let mut reference = BTreeMap::<Vec<u8>, Vec<u8>>::new();
        let mut snapshots = vec![reference.clone()];

        for sequence in 1..=ROUNDS {
            let repeated_key = KEYS[(next_random(&mut state) as usize) % KEYS.len()];
            let mut mutations = Vec::new();

            for ordinal in 0..4 {
                let key = if ordinal == 0 || ordinal == 3 {
                    repeated_key.to_vec()
                } else {
                    KEYS[(next_random(&mut state) as usize) % KEYS.len()].to_vec()
                };
                if next_random(&mut state) % 5 == 0 {
                    mutations.push(TabletMutation::Delete { key: key.clone() });
                    reference.remove(&key);
                } else {
                    let value = format!("{initial_seed:x}:{sequence}:{ordinal}").into_bytes();
                    mutations.push(TabletMutation::Put {
                        key: key.clone(),
                        value: value.clone(),
                    });
                    reference.insert(key, value);
                }
            }

            let memory_write = memory
                .prepare_write(5, sequence - 1, mutations.clone())
                .unwrap();
            let durable_write = durable
                .prepare_write(5, sequence - 1, mutations)
                .unwrap();
            assert_eq!(memory.commit(memory_write).unwrap(), sequence);
            assert_eq!(durable.commit(durable_write).unwrap(), sequence);
            snapshots.push(reference.clone());

            // Checkpoints reclaim the WAL; recovery must still support the
            // entire historical range retained by this prototype.
            if sequence % 8 == 0 {
                durable.checkpoint().unwrap();
            }
        }

        drop(durable);
        let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
        assert_eq!(reopened.current_sequence(), ROUNDS);

        let split = descriptor()
            .plan_split(
                b"m",
                TabletId::new(72).unwrap(),
                TabletId::new(73).unwrap(),
                6,
            )
            .unwrap();
        let (left, right) = reopened.materialize_split(&split).unwrap();
        assert_eq!(left.current_sequence(), ROUNDS);
        assert_eq!(right.current_sequence(), ROUNDS);

        for snapshot in 0..=ROUNDS {
            let expected = &snapshots[snapshot as usize];
            for key in KEYS {
                let value = expected.get(key).map(Vec::as_slice);
                assert_eq!(
                    memory.read_at(key, snapshot).unwrap(),
                    value,
                    "memory read: seed={initial_seed} snapshot={snapshot} key={key:?}"
                );
                assert_eq!(
                    reopened.read_at(key, snapshot).unwrap(),
                    value,
                    "recovery: seed={initial_seed} snapshot={snapshot} key={key:?}"
                );
                let child_value = if key < &b"m"[..] {
                    left.read_at(key, snapshot).unwrap()
                } else {
                    right.read_at(key, snapshot).unwrap()
                };
                assert_eq!(
                    child_value, value,
                    "split: seed={initial_seed} snapshot={snapshot} key={key:?}"
                );
            }

            for limit in [0, 1, 3, KEYS.len()] {
                let all = expected_rows(expected, b"a", b"z", limit);
                assert_eq!(
                    memory.scan_at(b"a", Some(b"z"), snapshot, limit).unwrap(),
                    all,
                    "memory scan: seed={initial_seed} snapshot={snapshot} limit={limit}"
                );
                assert_eq!(
                    reopened
                        .scan_at(b"a", Some(b"z"), snapshot, limit)
                        .unwrap(),
                    all,
                    "recovery scan: seed={initial_seed} snapshot={snapshot} limit={limit}"
                );

                assert_eq!(
                    left.scan_at(b"a", Some(b"m"), snapshot, limit).unwrap(),
                    expected_rows(expected, b"a", b"m", limit),
                    "left split scan: seed={initial_seed} snapshot={snapshot} limit={limit}"
                );
                assert_eq!(
                    right.scan_at(b"m", Some(b"z"), snapshot, limit).unwrap(),
                    expected_rows(expected, b"m", b"z", limit),
                    "right split scan: seed={initial_seed} snapshot={snapshot} limit={limit}"
                );
            }
        }

        drop(reopened);
        cleanup(&path);
    }
}
