//! Contract tests for a pull-based, per-tablet committed-transaction feed.
//!
//! The cursor is the *last consumed* sequence. A feed must never silently
//! skip reclaimed WAL history or expose a prepared/rejected write.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::changefeed::ChangefeedError;
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);
const EPOCH: u64 = 9;

struct Fixture {
    dir: PathBuf,
    wal: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nulang_changefeed_{}_{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let wal = dir.join("tablet.wal");
        Self { dir, wal }
    }

    fn open(&self) -> WalBackedTablet {
        WalBackedTablet::open(descriptor(), &self.wal).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(91).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        EPOCH,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn commit(tablet: &mut WalBackedTablet, mutations: Vec<TabletMutation>) -> u64 {
    let previous = tablet.current_sequence();
    let write = tablet
        .prepare_write(EPOCH, previous, mutations)
        .unwrap();
    tablet.commit(write).unwrap()
}

#[test]
fn feed_exposes_only_complete_committed_transactions_and_preserves_mutation_order() {
    let fixture = Fixture::new();
    let mut tablet = fixture.open();
    let rejected = tablet
        .prepare_write(EPOCH, 0, vec![put(b"q", b"rejected")])
        .unwrap();

    assert!(tablet.committed_after(0).unwrap().is_empty());

    let batch = vec![
        put(b"k", b"initial"),
        TabletMutation::Delete { key: b"k".to_vec() },
        put(b"k", b"final"),
        put(b"m", b"other"),
    ];
    assert_eq!(commit(&mut tablet, batch.clone()), 1);
    assert!(tablet.commit(rejected).is_err(), "stale write must be rejected");
    assert_eq!(commit(&mut tablet, vec![put(b"n", b"second")]), 2);

    let changes = tablet.committed_after(0).unwrap();
    assert_eq!(changes.len(), 2, "one event per committed transaction");
    assert_eq!(changes[0].sequence(), 1);
    assert_eq!(changes[0].ownership_epoch(), EPOCH);
    assert_eq!(changes[0].tablet_id(), TabletId::new(91).unwrap());
    assert_eq!(changes[0].mutations(), batch.as_slice());
    assert_eq!(changes[1].sequence(), 2);
    assert_eq!(changes[1].mutations(), &[put(b"n", b"second")]);

    assert_eq!(
        tablet.committed_after(1).unwrap().iter().map(|record| record.sequence()).collect::<Vec<_>>(),
        vec![2]
    );
    assert!(tablet.committed_after(2).unwrap().is_empty());
}

#[test]
fn checkpoint_publication_preserves_feed_but_reclamation_expires_old_cursors() {
    let fixture = Fixture::new();
    {
        let mut tablet = fixture.open();
        assert_eq!(commit(&mut tablet, vec![put(b"k", b"v1")]), 1);
        assert_eq!(commit(&mut tablet, vec![put(b"k", b"v2")]), 2);
        tablet.publish_checkpoint().unwrap();
        assert_eq!(tablet.committed_after(0).unwrap().len(), 2);
    }

    let mut tablet = fixture.open();
    assert_eq!(
        tablet.committed_after(0).unwrap().iter().map(|record| record.sequence()).collect::<Vec<_>>(),
        vec![1, 2],
        "reopen must reconstruct committed feed from durable WAL"
    );
    tablet.checkpoint().unwrap();

    assert_eq!(
        tablet.committed_after(0).unwrap_err(),
        ChangefeedError::HistoryPruned {
            requested_after: 0,
            oldest_available_after: 2
        },
        "an old cursor must not silently miss history"
    );
    assert!(tablet.committed_after(2).unwrap().is_empty());

    assert_eq!(commit(&mut tablet, vec![put(b"k", b"v3")]), 3);
    assert_eq!(tablet.committed_after(2).unwrap().len(), 1);
    drop(tablet);

    let tablet = fixture.open();
    assert_eq!(
        tablet.committed_after(1).unwrap_err(),
        ChangefeedError::HistoryPruned {
            requested_after: 1,
            oldest_available_after: 2
        }
    );
    assert_eq!(
        tablet.committed_after(2).unwrap().iter().map(|record| record.sequence()).collect::<Vec<_>>(),
        vec![3]
    );
}

#[test]
fn future_cursor_is_rejected_instead_of_being_interpreted_as_empty_feed() {
    let fixture = Fixture::new();
    let mut tablet = fixture.open();

    assert_eq!(
        tablet.committed_after(1).unwrap_err(),
        ChangefeedError::CursorAhead {
            requested_after: 1,
            latest: 0
        }
    );

    assert_eq!(commit(&mut tablet, vec![put(b"k", b"v1")]), 1);
    assert_eq!(
        tablet.committed_after(100).unwrap_err(),
        ChangefeedError::CursorAhead {
            requested_after: 100,
            latest: 1
        }
    );
}
