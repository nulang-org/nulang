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
use super::tablet::{KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation};
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
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}

// Full logical snapshots are deliberately different from the engine's
// per-key version chains. No engine read, sequence, or recovery outcome
// is used to construct the expected state.
struct Reference {
    sequence: u64,
    snapshots: Vec<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl Default for Reference {
    fn default() -> Self {
        Self {
            sequence: 0,
            snapshots: vec![BTreeMap::new()],
        }
    }
}

impl Reference {
    fn apply(&mut self, mutations: &[TabletMutation]) {
        let mut next = self.snapshots.last().unwrap().clone();
        for mutation in mutations {
            match mutation {
                TabletMutation::Put { key, value } => {
                    next.insert(key.clone(), value.clone());
                }
                TabletMutation::Delete { key } => {
                    next.remove(key);
                }
            }
        }
        self.sequence += 1;
        self.snapshots.push(next);
    }

    fn read_at(&self, key: &[u8], snapshot: u64) -> Option<Vec<u8>> {
        self.snapshots[snapshot as usize].get(key).cloned()
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
    for key in KEYS.chain(std::iter::once(b'x')) {
        for sequence in 0..=reference.sequence {
            let actual = tablet
                .read_at(&[key], sequence)
                .unwrap()
                .map(|value| value.to_vec());
            let expected = reference.read_at(&[key], sequence);
            assert_eq!(
                actual, expected,
                "seed={seed:#x}, step={step}, key={}, snapshot={sequence}",
                key as char
            );
        }
    }
}

#[test]
fn deterministic_recovery_matches_reference_across_seeded_histories() {
    for seed in [1_u64, 0x5eed, 0x1234_5678, 0xdec0_de01] {
        let path = temp_wal("seeded");
        cleanup(&path);
        let mut rng = Seeded(seed);
        let mut reference = Reference::default();
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

        use StorageInterruptionPoint::*;
        const WAL_POINTS: [StorageInterruptionPoint; 4] = [
            WalAfterHeader,
            WalAfterPayload,
            WalAfterChecksum,
            WalAfterSync,
        ];
        const CHECKPOINT_POINTS: [StorageInterruptionPoint; 4] = [
            CheckpointAfterTempWrite,
            CheckpointAfterTempSync,
            CheckpointAfterRename,
            CheckpointAfterDirectorySync,
        ];
        const RECLAIM_POINTS: [StorageInterruptionPoint; 4] = [
            WalReclaimAfterReplacementWrite,
            WalReclaimAfterReplacementSync,
            WalReclaimAfterRename,
            WalReclaimAfterDirectorySync,
        ];
        for step in 0..64 {
            // The first 12 steps cover every fault boundary for every seed.
            // Remaining actions are generated deterministically.
            let action = if step < 12 {
                10 + step as u64
            } else {
                rng.next() % 22
            };
            match action {
                0 => tablet.checkpoint().unwrap(),
                1 => tablet.publish_checkpoint().unwrap(),
                2 => {
                    drop(tablet);
                    tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
                }
                14..=21 => {
                    let point = if action < 18 {
                        CHECKPOINT_POINTS[(action - 14) as usize]
                    } else {
                        RECLAIM_POINTS[(action - 18) as usize]
                    };
                    // Commit fresh state so reclaim is meaningful.
                    let mutations = rng.mutations(seed, step);
                    let write = tablet
                        .prepare_write(EPOCH, reference.sequence, mutations.clone())
                        .unwrap();
                    assert_eq!(tablet.commit(write).unwrap(), reference.sequence + 1);
                    reference.apply(&mutations);
                    let result = with_interruption(point, || tablet.checkpoint());
                    let error = result.unwrap_err();
                    assert!(
                        error.to_string().contains(&format!("{point:?}")),
                        "seed={seed:#x}, step={step}: wrong checkpoint error {error}"
                    );
                    drop(tablet);
                    tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
                }
                _ => {
                    let mutations = rng.mutations(seed, step);
                    let before = reference.sequence;
                    let write = tablet
                        .prepare_write(EPOCH, before, mutations.clone())
                        .unwrap();
                    if (10..=13).contains(&action) {
                        let point = WAL_POINTS[(action - 10) as usize];
                        let error = with_interruption(point, || tablet.commit(write)).unwrap_err();
                        assert!(
                            error.to_string().contains(&format!("{point:?}")),
                            "seed={seed:#x}, step={step}: wrong append error {error}"
                        );
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
                        // These are injected I/O errors, not power loss: a complete
                        // frame is visible on reopen; incomplete frames are not.
                        // Never derive the reference model from the recovered tail.
                        if matches!(point, WalAfterChecksum | WalAfterSync) {
                            reference.apply(&mutations);
                        }
                    } else {
                        let stale = tablet
                            .prepare_write(
                                EPOCH,
                                before,
                                vec![TabletMutation::Put {
                                    key: b"x".to_vec(),
                                    value: b"must-not-commit".to_vec(),
                                }],
                            )
                            .unwrap();
                        let committed = tablet.commit(write).unwrap();
                        reference.apply(&mutations);
                        assert_eq!(
                            committed, reference.sequence,
                            "seed={seed:#x}, step={step}: acknowledged wrong sequence"
                        );
                        assert_eq!(
                            tablet.commit(stale).unwrap_err(),
                            WalBackedError::Tablet(TabletError::SequenceMismatch {
                                committed,
                                expected_previous: before
                            }),
                            "seed={seed:#x}, step={step}: stale write was not rejected"
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
    assert!(
        with_interruption(StorageInterruptionPoint::WalAfterSync, || {
            tablet.commit(write)
        })
        .is_err()
    );
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

#[cfg(unix)]
mod managed_model {
    use super::super::generation_registry::{GenerationRegistry, GenerationStatus};
    use super::super::managed_storage::ManagedTablet;
    use super::super::sst_manifest::ManifestStore;
    use super::super::tablet::{TransactionId, TransactionReceipt};
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = temp_wal("managed").with_extension("dir");
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir(&path).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn manifest(path: &Path) -> ManifestStore {
        ManifestStore::create(path, 127, EPOCH).unwrap()
    }
    fn reopen(wal: &Path, active: &Path, reg: &Path) -> ManagedTablet {
        ManagedTablet::open(
            descriptor(),
            wal,
            active,
            GenerationRegistry::open(reg, 127, EPOCH).unwrap(),
        )
        .unwrap()
    }
    fn tx(sequence: u64) -> TransactionId {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&sequence.to_le_bytes());
        TransactionId::new(bytes)
    }
    type Ledger = Vec<(Vec<TabletMutation>, TransactionReceipt)>;
    fn check(
        db: &mut ManagedTablet,
        model: &Reference,
        ledger: &Ledger,
        floor: u64,
        seed: u64,
        step: usize,
    ) {
        assert_eq!(
            db.current_sequence(),
            model.sequence,
            "seed={seed:x} step={step} sequence"
        );
        assert_eq!(
            db.oldest_retained_snapshot(),
            floor,
            "seed={seed:x} step={step} floor"
        );
        for key in KEYS.chain(std::iter::once(b'x')) {
            for seq in floor..=model.sequence {
                assert_eq!(
                    db.read_at(&[key], seq).unwrap().map(<[u8]>::to_vec),
                    model.read_at(&[key], seq),
                    "seed={seed:x} step={step} key={key} seq={seq}"
                );
            }
            if floor > 0 {
                assert!(db.read_at(&[key], floor - 1).is_err());
            }
            assert!(db.read_at(&[key], model.sequence + 1).is_err());
        }
        for (i, (mutations, receipt)) in ledger.iter().enumerate() {
            assert_eq!(
                db.commit_idempotent(tx(i as u64 + 1), mutations.clone())
                    .unwrap(),
                *receipt,
                "seed={seed:x} step={step} receipt={i}"
            );
        }
        assert_eq!(db.current_sequence(), model.sequence);
    }
    fn put_new(
        db: &mut ManagedTablet,
        model: &mut Reference,
        ledger: &mut Ledger,
        mutations: Vec<TabletMutation>,
    ) {
        let next = model.sequence + 1;
        let receipt = db.commit_idempotent(tx(next), mutations.clone()).unwrap();
        assert_eq!(receipt.sequence(), next);
        model.apply(&mutations);
        ledger.push((mutations, receipt));
    }
    #[test]
    fn seeded_managed_lifecycle_matches_independent_snapshot_and_receipt_model() {
        use StorageInterruptionPoint::*;
        const SEEDS: [u64; 8] = [
            1,
            0x5eed,
            0x12345678,
            0xdec0de01,
            0xcafe,
            0xabad1dea,
            0x987654321,
            0xdeadbeef,
        ];
        const WRITE_FAULTS: [StorageInterruptionPoint; 4] = [
            WalAfterHeader,
            WalAfterPayload,
            WalAfterChecksum,
            WalAfterSync,
        ];
        const ROTATION_FAULTS: [StorageInterruptionPoint; 5] = [
            RegistryBeforeRename,
            RegistryAfterRename,
            WalReclaimAfterReplacementSync,
            WalReclaimAfterRename,
            WalReclaimAfterDirectorySync,
        ];
        for seed in SEEDS {
            let fixture = Fixture::new();
            let wal = fixture.0.join("tablet.wal");
            let reg = fixture.dir("registry");
            let mut active = fixture.dir("initial");
            let mut rng = Seeded(seed);
            let mut model = Reference::default();
            let mut ledger = Ledger::new();
            let mut raw = WalBackedTablet::open(descriptor(), &wal).unwrap();
            for step in 0..4 {
                let mutations = rng.mutations(seed, step);
                let receipt = raw
                    .commit_idempotent(tx(model.sequence + 1), mutations.clone())
                    .unwrap();
                model.apply(&mutations);
                assert_eq!(receipt.sequence(), model.sequence);
                ledger.push((mutations, receipt));
            }
            let mut initial = manifest(&active);
            for _ in 0..4 {
                raw.flush_sst_shadow(&mut initial, 1).unwrap();
            }
            let mut floor = 2;
            raw.seal_sst_recovery_at_floor(&mut initial, floor).unwrap();
            raw.reclaim_to_sealed_sst(&initial).unwrap();
            let registry = GenerationRegistry::create(&reg, 127, EPOCH).unwrap();
            let mut db = ManagedTablet::adopt(raw, registry, initial).unwrap();
            check(&mut db, &model, &ledger, floor, seed, 0);
            for round in 0..6 {
                for slot in 0..4 {
                    let step = 4 + round * 4 + slot;
                    let mutations = rng.mutations(seed, step);
                    let next = model.sequence + 1;
                    if slot == 1 || slot == 3 {
                        let point = WRITE_FAULTS[((rng.next() as usize) + round + slot) % 4];
                        let failure = with_interruption(point, || {
                            db.commit_idempotent(tx(next), mutations.clone())
                        });
                        assert!(
                            failure
                                .unwrap_err()
                                .to_string()
                                .contains(&format!("{point:?}")),
                            "seed={seed:x} step={step}"
                        );
                        assert_eq!(db.current_sequence(), model.sequence);
                        assert!(db.commit_idempotent(tx(next), mutations.clone()).is_err());
                        drop(db);
                        db = reopen(&wal, &active, &reg);
                        let survived = matches!(point, WalAfterChecksum | WalAfterSync);
                        assert_eq!(
                            db.current_sequence(),
                            model.sequence + u64::from(survived),
                            "seed={seed:x} step={step}"
                        );
                    }
                    put_new(&mut db, &mut model, &mut ledger, mutations);
                    if slot == 2 {
                        drop(db);
                        db = reopen(&wal, &active, &reg);
                    }
                    check(&mut db, &model, &ledger, floor, seed, step);
                }
                let requested = model.sequence - (rng.next() % 3);
                let point = ROTATION_FAULTS[round % ROTATION_FAULTS.len()];
                let next = fixture.dir(&format!("rotate-{round}"));
                let failure = with_interruption(point, || db.rotate(manifest(&next), requested));
                assert!(
                    failure
                        .unwrap_err()
                        .to_string()
                        .contains(&format!("{point:?}")),
                    "seed={seed:x} round={round}"
                );
                drop(db);
                if matches!(point, WalReclaimAfterRename | WalReclaimAfterDirectorySync) {
                    active = next;
                    floor = requested;
                    db = reopen(&wal, &active, &reg);
                } else {
                    db = reopen(&wal, &active, &reg);
                    check(&mut db, &model, &ledger, floor, seed, 100 + round);
                    let retry = fixture.dir(&format!("rotate-retry-{round}"));
                    db.rotate(manifest(&retry), requested).unwrap();
                    active = retry;
                    floor = requested;
                }
                check(&mut db, &model, &ledger, floor, seed, 200 + round);
                // Preserve an acknowledged nonempty tail across compaction.
                let mutations = rng.mutations(seed, 100 + round);
                put_new(&mut db, &mut model, &mut ledger, mutations);
                let predecessor = active.clone();
                let compact = fixture.dir(&format!("compact-{round}"));
                let bytes = fs::read(&wal).unwrap();
                if round % 2 == 0 {
                    let partial = fixture.dir(&format!("partial-{round}"));
                    let failure =
                        with_interruption(CompactionAfterOutput, || db.compact(manifest(&partial)));
                    assert!(failure
                        .unwrap_err()
                        .to_string()
                        .contains("CompactionAfterOutput"));
                    assert_eq!(fs::read(&wal).unwrap(), bytes);
                    drop(db);
                    db = reopen(&wal, &active, &reg);
                    check(&mut db, &model, &ledger, floor, seed, 300 + round);
                }
                db.compact(manifest(&compact)).unwrap();
                active = compact;
                assert_eq!(&fs::read(&wal).unwrap()[128..], &bytes[128..]);
                drop(db);
                db = reopen(&wal, &active, &reg);
                check(&mut db, &model, &ledger, floor, seed, 400 + round);
                let backup = fixture.0.join(format!("backup-{round}"));
                let q = fixture.0.join(format!("quarantine-{round}"));
                db.backup([round as u8 + 1; 16], &backup).unwrap();
                db.quarantine(
                    ManifestStore::open(&predecessor, 127, EPOCH).unwrap(),
                    &backup,
                    &q,
                )
                .unwrap();
                let old = ManifestStore::open(&q, 127, EPOCH).unwrap();
                db.reserve_retirement(&old, &backup).unwrap();
                drop(old);
                db.delete_reserved_generation(&q, &backup).unwrap();
                assert_eq!(db.retirement_status(&q).unwrap(), GenerationStatus::Deleted);
                let mut restored = WalBackedTablet::restore_sealed_backup(
                    descriptor(),
                    &backup,
                    fixture.0.join(format!("restored-{round}")),
                )
                .unwrap();
                assert_eq!(restored.current_sequence(), model.sequence);
                assert_eq!(restored.oldest_retained_snapshot(), floor);
                for key in KEYS {
                    for seq in floor..=model.sequence {
                        assert_eq!(
                            restored.read_at(&[key], seq).unwrap().map(<[u8]>::to_vec),
                            model.read_at(&[key], seq)
                        );
                    }
                }
                for (i, (mutations, receipt)) in ledger.iter().enumerate() {
                    assert_eq!(
                        restored
                            .commit_idempotent(tx(i as u64 + 1), mutations.clone())
                            .unwrap(),
                        *receipt
                    );
                }
                check(&mut db, &model, &ledger, floor, seed, 500 + round);
            }
        }
    }
}
