use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::FileWal;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_checkpoint_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn checkpoint_path(wal: &PathBuf) -> PathBuf {
    wal.with_extension("checkpoint")
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(71).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap()
}

fn commit_put(tablet: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
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
fn checkpoint_reclaims_wal_prefix_and_restart_restores_mvcc_history() {
    let wal_path = temp_wal("restart");
    let checkpoint = checkpoint_path(&wal_path);
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        commit_put(&mut tablet, b"k", b"v1");
        commit_put(&mut tablet, b"k", b"v2");
        commit_put(&mut tablet, b"other", b"x");

        tablet.checkpoint().unwrap();

        assert_eq!(tablet.current_sequence(), 3);
        let wal = FileWal::open(&wal_path).unwrap();
        assert_eq!(wal.base_sequence(), 3);
        assert!(wal.records().is_empty());
    }

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        assert_eq!(tablet.current_sequence(), 3);
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
        assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
        assert_eq!(tablet.read_latest(b"other"), Some(&b"x"[..]));

        commit_put(&mut tablet, b"k", b"v4");
        assert_eq!(tablet.current_sequence(), 4);
    }

    let reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(reopened.current_sequence(), 4);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(reopened.read_at(b"k", 3).unwrap(), Some(&b"v2"[..]));
    assert_eq!(reopened.read_at(b"k", 4).unwrap(), Some(&b"v4"[..]));

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);
}

#[test]
fn published_checkpoint_is_usable_even_when_wal_prefix_was_not_reclaimed() {
    let wal_path = temp_wal("full_wal");
    let checkpoint = checkpoint_path(&wal_path);
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        commit_put(&mut tablet, b"k", b"v1");
        commit_put(&mut tablet, b"k", b"v2");
        tablet.publish_checkpoint().unwrap();
    }

    // Simulates a crash after atomic checkpoint publication but before the WAL
    // rewrite/reclamation step. Recovery must not double-apply records 1..2.
    let tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);
}

#[test]
fn corrupt_checkpoint_fails_closed_instead_of_falling_back_to_compacted_wal() {
    let wal_path = temp_wal("corrupt");
    let checkpoint = checkpoint_path(&wal_path);
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        commit_put(&mut tablet, b"k", b"value");
        tablet.checkpoint().unwrap();
    }

    let mut bytes = fs::read(&checkpoint).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&checkpoint, bytes).unwrap();

    assert!(WalBackedTablet::open(descriptor(), &wal_path).is_err());

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);
}

#[test]
fn checkpoint_ahead_of_wal_tail_fails_closed() {
    let wal_path = temp_wal("checkpoint_ahead");
    let checkpoint = checkpoint_path(&wal_path);
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        commit_put(&mut tablet, b"k", b"value");
        tablet.publish_checkpoint().unwrap();
    }

    // Simulate loss/rollback of the WAL after checkpoint publication. A
    // checkpoint at sequence 1 cannot be paired with a durable WAL tail at 0.
    fs::remove_file(&wal_path).unwrap();
    FileWal::open(&wal_path).unwrap();

    assert!(WalBackedTablet::open(descriptor(), &wal_path).is_err());

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);
}
#[test]
fn repeated_key_mutations_survive_wal_replay_and_checkpoint_reclamation() {
    let wal_path = temp_wal("repeated_key");
    let checkpoint = checkpoint_path(&wal_path);
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);

    // One atomic tablet write may contain repeated keys. The final mutation
    // determines the visible value at that sequence, including tombstones.
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
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
                        key: b"other".to_vec(),
                        value: b"kept".to_vec(),
                    },
                    TabletMutation::Delete { key: b"k".to_vec() },
                    TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"final".to_vec(),
                    },
                ],
            )
            .unwrap();
        tablet.commit(write).unwrap();
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"final"[..]));
    }

    // Replay the original WAL before creating a checkpoint. This must not
    // duplicate MVCC sequence numbers for the repeated key.
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"final"[..]));
        tablet.checkpoint().unwrap();
        let wal = FileWal::open(&wal_path).unwrap();
        assert_eq!(wal.base_sequence(), 1);
    }

    // A checkpoint must reopen successfully after the WAL history was
    // reclaimed, preserving both the final write and its sequence.
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"final"[..]));
        assert_eq!(tablet.read_latest(b"other"), Some(&b"kept"[..]));

        let write = tablet
            .prepare_write(
                3,
                1,
                vec![
                    TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"temporary".to_vec(),
                    },
                    TabletMutation::Delete { key: b"k".to_vec() },
                ],
            )
            .unwrap();
        tablet.commit(write).unwrap();
        assert_eq!(tablet.read_latest(b"k"), None);
        tablet.checkpoint().unwrap();
    }

    let reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"final"[..]));
    assert_eq!(reopened.read_at(b"k", 2).unwrap(), None);
    assert_eq!(reopened.read_latest(b"other"), Some(&b"kept"[..]));

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);
}

