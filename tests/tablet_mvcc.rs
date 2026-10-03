use std::collections::BTreeMap;

use nulang::database::tablet::{
    KeyRange, MemoryTablet, MvccStorage, Tablet, TabletDescriptor, TabletError, TabletId,
    TabletMutation, TabletWrite,
};

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(21).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

#[derive(Debug, Default)]
struct TestStorage {
    current_sequence: u64,
    rows: BTreeMap<Vec<u8>, Vec<(u64, Option<Vec<u8>>)>>,
}

impl MvccStorage for TestStorage {
    fn current_sequence(&self) -> u64 {
        self.current_sequence
    }

    fn apply_committed(&mut self, sequence: u64, mutations: Vec<TabletMutation>) {
        for mutation in mutations {
            match mutation {
                TabletMutation::Put { key, value } => {
                    self.rows
                        .entry(key)
                        .or_default()
                        .push((sequence, Some(value)));
                }
                TabletMutation::Delete { key } => {
                    self.rows.entry(key).or_default().push((sequence, None));
                }
            }
        }
        self.current_sequence = sequence;
    }

    fn read_at<'a>(&'a self, key: &[u8], snapshot: u64) -> Option<&'a [u8]> {
        self.rows.get(key).and_then(|versions| {
            versions
                .iter()
                .rev()
                .find(|(sequence, _)| *sequence <= snapshot)
                .and_then(|(_, value)| value.as_deref())
        })
    }
}

#[test]
fn tablet_can_use_a_storage_engine_behind_the_mvcc_contract() {
    let mut tablet = Tablet::with_storage(descriptor(), TestStorage::default());

    let first = tablet
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"v1".to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(first).unwrap();
    let first_snapshot = tablet.current_sequence();

    let second = tablet
        .prepare_write(
            1,
            first_snapshot,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"v2".to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(second).unwrap();

    assert_eq!(
        tablet.read_at(b"k", first_snapshot).unwrap(),
        Some(&b"v1"[..])
    );
    assert_eq!(tablet.read_latest(b"k"), Some(&b"v2"[..]));
}

#[test]
fn mvcc_reads_preserve_committed_snapshots_across_updates() {
    let mut tablet = MemoryTablet::new(descriptor());

    let first = tablet
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"v1".to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(first).unwrap();
    let snapshot = tablet.current_sequence();

    let second = tablet
        .prepare_write(
            1,
            1,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"v2".to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(second).unwrap();

    assert_eq!(tablet.read_at(b"k", snapshot).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_latest(b"k"), Some(&b"v2"[..]));
}

#[test]
fn mvcc_deletes_are_tombstones_without_destroying_older_snapshots() {
    let mut tablet = MemoryTablet::new(descriptor());

    let put = tablet
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"value".to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(put).unwrap();

    let delete = tablet
        .prepare_write(1, 1, vec![TabletMutation::Delete { key: b"k".to_vec() }])
        .unwrap();
    tablet.commit(delete).unwrap();

    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"value"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), None);
}

#[test]
fn committing_a_stale_prepared_write_is_rejected() {
    let mut tablet = MemoryTablet::new(descriptor());

    let first = tablet
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"a1".to_vec(),
                value: b"first".to_vec(),
            }],
        )
        .unwrap();
    let stale = tablet
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"a2".to_vec(),
                value: b"stale".to_vec(),
            }],
        )
        .unwrap();

    tablet.commit(first).unwrap();

    assert_eq!(
        tablet.commit(stale).unwrap_err(),
        TabletError::SequenceMismatch {
            committed: 1,
            expected_previous: 0,
        }
    );
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_latest(b"a2"), None);
}

#[test]
fn commit_revalidates_range_before_mutating_any_state() {
    let narrow = TabletDescriptor::new(
        TabletId::new(21).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"m".to_vec())).unwrap(),
        1,
    )
    .unwrap();
    let wider = descriptor();
    let mut tablet = MemoryTablet::new(narrow);

    let write = TabletWrite::prepare(
        &wider,
        1,
        0,
        0,
        vec![
            TabletMutation::Put {
                key: b"b".to_vec(),
                value: b"inside".to_vec(),
            },
            TabletMutation::Put {
                key: b"y".to_vec(),
                value: b"outside".to_vec(),
            },
        ],
    )
    .unwrap();

    assert_eq!(
        tablet.commit(write).unwrap_err(),
        TabletError::KeyOutsideTabletRange
    );
    assert_eq!(tablet.current_sequence(), 0);
    assert_eq!(tablet.read_latest(b"b"), None);
}

#[test]
fn reads_reject_snapshots_that_have_not_committed() {
    let tablet = MemoryTablet::new(descriptor());

    assert_eq!(
        tablet.read_at(b"k", 1).unwrap_err(),
        TabletError::SnapshotAhead {
            committed: 0,
            requested: 1,
        }
    );
}
