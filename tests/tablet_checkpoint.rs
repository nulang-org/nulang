use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::FileWal;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn paths(name: &str) -> (PathBuf, PathBuf) {
    let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!(
        "nulang_nudb_checkpoint_{name}_{}_{}",
        std::process::id(),
        id
    ));
    (base.with_extension("wal"), base.with_extension("checkpoint"))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(71).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap()
}

fn put(tablet: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
    let sequence = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            3,
            sequence,
            vec![TabletMutation::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

#[test]
fn checkpoint_reclaims_wal_prefix_and_restart_replays_newer_tail() {
    let (wal_path, checkpoint_path) = paths("reclaim");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);

    {
        let mut tablet =
            WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path)
                .unwrap();
        put(&mut tablet, b"k", b"v1");
        put(&mut tablet, b"k", b"v2");
        tablet.checkpoint(&checkpoint_path).unwrap();
        put(&mut tablet, b"k", b"v3");
    }

    let wal = FileWal::open(&wal_path).unwrap();
    assert_eq!(wal.base_sequence(), 1);
    assert_eq!(wal.records().len(), 2);
    assert_eq!(wal.records()[0].sequence(), 2);
    assert_eq!(wal.records()[1].sequence(), 3);
    drop(wal);

    let tablet =
        WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path).unwrap();
    assert_eq!(tablet.current_sequence(), 3);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"v3"[..]));

    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(checkpoint_path);
}

#[test]
fn checkpoint_can_recover_with_an_unreclaimed_full_wal() {
    let (wal_path, checkpoint_path) = paths("full_wal");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        put(&mut tablet, b"k1", b"one");
        put(&mut tablet, b"k2", b"two");
        tablet.write_checkpoint_only(&checkpoint_path).unwrap();
        put(&mut tablet, b"k3", b"three");
    }

    let wal = FileWal::open(&wal_path).unwrap();
    assert_eq!(wal.base_sequence(), 0);
    assert_eq!(wal.records().len(), 3);
    drop(wal);

    let tablet =
        WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path).unwrap();
    assert_eq!(tablet.current_sequence(), 3);
    assert_eq!(tablet.read_latest(b"k1"), Some(&b"one"[..]));
    assert_eq!(tablet.read_latest(b"k2"), Some(&b"two"[..]));
    assert_eq!(tablet.read_latest(b"k3"), Some(&b"three"[..]));

    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(checkpoint_path);
}

#[test]
fn checkpoint_corruption_fails_closed() {
    let (wal_path, checkpoint_path) = paths("corrupt");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);

    {
        let mut tablet =
            WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path)
                .unwrap();
        put(&mut tablet, b"k", b"value");
        tablet.checkpoint(&checkpoint_path).unwrap();
    }

    let mut bytes = fs::read(&checkpoint_path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x5a;
    fs::write(&checkpoint_path, bytes).unwrap();

    assert!(WalBackedTablet::open_with_checkpoint(
        descriptor(),
        &wal_path,
        &checkpoint_path
    )
    .is_err());

    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(checkpoint_path);
}

#[test]
fn compacted_wal_without_checkpoint_refuses_state_recovery() {
    let (wal_path, checkpoint_path) = paths("missing_checkpoint");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);

    {
        let mut tablet =
            WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path)
                .unwrap();
        put(&mut tablet, b"k", b"v1");
        put(&mut tablet, b"k", b"v2");
        tablet.checkpoint(&checkpoint_path).unwrap();
    }

    fs::remove_file(&checkpoint_path).unwrap();

    assert!(WalBackedTablet::open(descriptor(), &wal_path).is_err());

    let _ = fs::remove_file(wal_path);
}


#[test]
fn repeated_checkpoints_preserve_tombstone_history_and_advance_the_wal_anchor() {
    let (wal_path, checkpoint_path) = paths("repeat");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);

    {
        let mut tablet =
            WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path)
                .unwrap();

        put(&mut tablet, b"k", b"v1");

        let delete = tablet
            .prepare_write(
                3,
                tablet.current_sequence(),
                vec![TabletMutation::Delete { key: b"k".to_vec() }],
            )
            .unwrap();
        tablet.commit(delete).unwrap();
        tablet.checkpoint(&checkpoint_path).unwrap();

        put(&mut tablet, b"k", b"v3");
        put(&mut tablet, b"x", b"other");
        tablet.checkpoint(&checkpoint_path).unwrap();
    }

    let wal = FileWal::open(&wal_path).unwrap();
    assert_eq!(wal.base_sequence(), 3);
    assert_eq!(wal.records().len(), 1);
    assert_eq!(wal.records()[0].sequence(), 4);
    drop(wal);

    let tablet =
        WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path).unwrap();
    assert_eq!(tablet.current_sequence(), 4);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), None);
    assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"v3"[..]));
    assert_eq!(tablet.read_latest(b"x"), Some(&b"other"[..]));

    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(checkpoint_path);
}


#[test]
fn incomplete_checkpoint_temp_file_is_ignored_after_crash_before_rename() {
    let (wal_path, checkpoint_path) = paths("temp_crash");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);

    {
        let mut tablet =
            WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path)
                .unwrap();
        put(&mut tablet, b"k", b"v1");
        tablet.checkpoint(&checkpoint_path).unwrap();
        put(&mut tablet, b"k", b"v2");
    }

    let checkpoint_name = checkpoint_path.file_name().unwrap().to_string_lossy();
    let temp_path = checkpoint_path.with_file_name(format!(".{checkpoint_name}.tmp"));
    fs::write(&temp_path, b"partial checkpoint bytes").unwrap();

    let tablet =
        WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path).unwrap();
    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));

    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(checkpoint_path);
    let _ = fs::remove_file(temp_path);
}


#[test]
fn checkpoint_preserves_multiple_mutations_to_one_key_in_one_commit() {
    let (wal_path, checkpoint_path) = paths("same_sequence_versions");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);

    {
        let mut tablet =
            WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path)
                .unwrap();
        let write = tablet
            .prepare_write(
                3,
                0,
                vec![
                    TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"first".to_vec(),
                    },
                    TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"last".to_vec(),
                    },
                ],
            )
            .unwrap();
        tablet.commit(write).unwrap();
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"last"[..]));
        tablet.checkpoint(&checkpoint_path).unwrap();
    }

    let tablet =
        WalBackedTablet::open_with_checkpoint(descriptor(), &wal_path, &checkpoint_path).unwrap();
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"last"[..]));

    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(checkpoint_path);
}
