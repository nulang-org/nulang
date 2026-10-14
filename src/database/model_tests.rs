//! Seeded, model-based tests of WAL/MVCC/checkpoint/restart behavior.
//!
//! The reference model deliberately does not use NuDB's MVCC implementation.
//! A printed seed and step make failures repeatable without timing assumptions.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::interruption::{with_interruption, StorageInterruptionPoint};
use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use super::wal::WalError;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);
const EPOCH: u64 = 7;
const KEYS: std::ops::RangeInclusive<u8> = b'b'..=b'i';

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(127).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        EPOCH,
    )
    .unwrap()
}

fn temp_wal(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_model_{label}_{}_{}.wal",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let mut reclaim_temp = path.as_os_str().to_os_string();
    reclaim_temp.push(".reclaim.tmp");
    let _ = fs::remove_file(PathBuf::from(reclaim_temp));
    let mut checkpoint_temp = path.with_extension("checkpoint").into_os_string();
    checkpoint_temp.push(".tmp");
    let _ = fs::remove_file(PathBuf::from(checkpoint_temp));
}

#[derive(Default)]
struct Reference {
    sequence: u64,
    // One logical version per key/transaction. Last mutation in the batch wins.
    versions: BTreeMap<Vec<u8>, Vec<(u64, Option<Vec<u8>>)>>,
}

impl Reference {
    fn apply(&mut self, mutations: &[TabletMutation]) {
        self.sequence += 1;
        let mut final_values = BTreeMap::new();
        for mutation in mutations {
            match mutation {
                TabletMutation::Put { key, value } => {
                    final_values.insert(key.clone(), Some(value.clone()));
                }
                TabletMutation::Delete { key } => {
                    final_values.insert(key.clone(), None);
                }
            }
        }
        for (key, value) in final_values {
            self.versions
                .entry(key)
                .or_default()
                .push((self.sequence, value));
        }
    }

    fn read_at(&self, key: &[u8], snapshot: u64) -> Option<Vec<u8>> {
        self.versions.get(key).and_then(|versions| {
            versions
                .iter()
                .rev()
                .find(|(sequence, _)| *sequence <= snapshot)
                .and_then(|(_, value)| value.clone())
        })
    }
}

struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        // A fixed PRNG, not thread_rng: seed + step identify every failure.
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn mutations(&mut self, seed: u64, step: usize) -> Vec<TabletMutation> {
        let count = 1 + (self.next() % 3) as usize;
        (0..count)
            .map(|slot| {
                let key = vec![b'b' + (self.next() % 8) as u8];
                let choice = self.next();
                if choice % 5 == 0 {
                    TabletMutation::Delete { key }
                } else {
                    TabletMutation::Put {
                        key,
                        value: format!("{seed:x}:{step}:{slot}:{choice:x}").into_bytes(),
                    }
                }
            })
            .collect()
    }
}

fn assert_matches_reference(
    tablet: &WalBackedTablet,
    reference: &Reference,
    seed: u64,
    step: usize,
) {
    assert_eq!(
        tablet.current_sequence(),
        reference.sequence,
        "seed={seed:#x}, step={step}: transaction sequence"
    );

    // Compare every historical snapshot, not just latest state. This detects
    // lost versions, replayed transactions and delete/reinsert mistakes.
    for key in KEYS {
        for sequence in 0..=reference.sequence {
            let actual = tablet
                .read_at(&[key], sequence)
                .unwrap()
                .map(|value| value.to_vec());
            let expected = reference.read_at(&[key], sequence);
            assert_eq!(
                actual,
                expected,
                "seed={seed:#x}, step={step}, key={}, snapshot={sequence}",
                key as char
            );
        }
    }
}

#[test]
fn multiple_mutations_of_one_key_have_one_recoverable_version_per_commit() {
    let path = temp_wal("duplicate_key");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let mutations = vec![
        TabletMutation::Put {
            key: b"k".to_vec(),
            value: b"first".to_vec(),
        },
        TabletMutation::Delete { key: b"k".to_vec() },
        TabletMutation::Put {
            key: b"k".to_vec(),
            value: b"last".to_vec(),
        },
    ];
    let write = tablet.prepare_write(EPOCH, 0, mutations).unwrap();
    assert_eq!(tablet.commit(write).unwrap(), 1);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"last"[..]));

    // Previously a checkpoint stored multiple versions at the same sequence,
    // but checkpoint restore rejected non-strictly-increasing version numbers.
    tablet.checkpoint().unwrap();
    drop(tablet);
    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"last"[..]));
    drop(reopened);
    cleanup(&path);
}

#[test]
fn deterministic_recovery_matches_reference_across_seeded_histories() {
    for seed in [1_u64, 0x5eed, 0x1234_5678, 0xdec0_de01] {
        let path = temp_wal("seeded");
        cleanup(&path);
        let mut rng = Seeded(seed);
        let mut reference = Reference::default();
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

        for step in 0..64 {
            let action = rng.next() % 10;
            match action {
                0 => tablet.checkpoint().unwrap(),
                1 => tablet.publish_checkpoint().unwrap(),
                2 => {
                    drop(tablet);
                    tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
                }
                _ => {
                    let mutations = rng.mutations(seed, step);
                    let before = reference.sequence;
                    let write = tablet
                        .prepare_write(EPOCH, before, mutations.clone())
                        .unwrap();

                    if action == 3 {
                        let point = match rng.next() % 4 {
                            0 => StorageInterruptionPoint::WalAfterHeader,
                            1 => StorageInterruptionPoint::WalAfterPayload,
                            2 => StorageInterruptionPoint::WalAfterChecksum,
                            _ => StorageInterruptionPoint::WalAfterSync,
                        };
                        let result = with_interruption(point, || tablet.commit(write));
                        assert!(
                            result.is_err(),
                            "seed={seed:#x}, step={step}: injected {point:?} returned success"
                        );
                        // Once bytes may have reached the WAL, the live handle
                        // cannot safely retry before it is reopened/reconciled.
                        let retry = tablet
                            .prepare_write(EPOCH, before, mutations.clone())
                            .unwrap();
                        assert_eq!(
                            tablet.commit(retry).unwrap_err(),
                            WalBackedError::Wal(WalError::Poisoned),
                            "seed={seed:#x}, step={step}: poisoned WAL accepted retry"
                        );
                        drop(tablet);
                        tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
                        let recovered = tablet.current_sequence();
                        assert!(
                            recovered == before || recovered == before + 1,
                            "seed={seed:#x}, step={step}: invalid recovery sequence {recovered}, before {before}"
                        );
                        // A returned error is NOT proof of abort: an intact
                        // record may be recovered after an injected failure.
                        if recovered == before + 1 {
                            reference.apply(&mutations);
                        }
                    } else {
                        let committed = tablet.commit(write).unwrap();
                        reference.apply(&mutations);
                        assert_eq!(
                            committed, reference.sequence,
                            "seed={seed:#x}, step={step}: acknowledged wrong sequence"
                        );
                    }
                }
            }
            assert_matches_reference(&tablet, &reference, seed, step);
        }

        // Final checkpoint/reclamation and reopen must preserve all history.
        tablet.checkpoint().unwrap();
        drop(tablet);
        let tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        assert_matches_reference(&tablet, &reference, seed, 64);
        drop(tablet);
        cleanup(&path);
    }
}

#[test]
fn failed_write_requires_reconciliation_before_retry() {
    let path = temp_wal("unknown_outcome");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let mutations = vec![TabletMutation::Put {
        key: b"k".to_vec(),
        value: b"value".to_vec(),
    }];
    let write = tablet.prepare_write(EPOCH, 0, mutations.clone()).unwrap();

    // A complete WAL frame exists, but the caller got an error rather than
    // a commit receipt. Reopen to determine whether its sequence committed.
    assert!(with_interruption(StorageInterruptionPoint::WalAfterSync, || {
        tablet.commit(write)
    })
    .is_err());
    drop(tablet);

    let mut recovered = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(recovered.current_sequence(), 1);
    assert_eq!(recovered.read_latest(b"k"), Some(&b"value"[..]));

    // The original prepared write cannot be appended again.
    let original = recovered.prepare_write(EPOCH, 0, mutations);
    assert!(original.is_err());
    let next = recovered
        .prepare_write(
            EPOCH,
            1,
            vec![TabletMutation::Put {
                key: b"j".to_vec(),
                value: b"next".to_vec(),
            }],
        )
        .unwrap();
    assert_eq!(recovered.commit(next).unwrap(), 2);
    drop(recovered);
    cleanup(&path);
}
