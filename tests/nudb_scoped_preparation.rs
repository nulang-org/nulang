//! Exercise pure scoped preparation separately from ordered durable commits.
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation, TransactionId,
};
use nulang::scoped_tasks::{
    run_scoped_tasks_with_budget, CancellationToken, ScopedTask, ScopedTaskFailure,
    SharedTaskBudget,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nudb_scoped_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn open(&self) -> WalBackedTablet {
        WalBackedTablet::open(
            TabletDescriptor::new(
                TabletId::new(1).unwrap(),
                KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
                1,
            )
            .unwrap(),
            self.0.join("tablet.wal"),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(n: u8) -> TransactionId {
    TransactionId::new([n; 16])
}

#[test]
fn scoped_preparation_preserves_owner_order_and_conflict_validation() {
    let fixture = Fixture::new();
    let mut owner = fixture.open();
    let snapshot = owner.current_sequence();
    let budget = SharedTaskBudget::new(2).unwrap();
    // The children own only computation inputs, never the tablet or WAL.
    let tasks: Vec<ScopedTask<'_, _, ()>> = [b"a", b"b", b"a"]
        .into_iter()
        .enumerate()
        .map(|(i, key)| {
            let task: ScopedTask<'_, _, ()> = Box::new(move |_| {
                Ok((
                    id(i as u8 + 1),
                    vec![TabletMutation::Put {
                        key: key.to_vec(),
                        value: vec![i as u8],
                    }],
                ))
            });
            task
        })
        .collect();
    let results =
        run_scoped_tasks_with_budget(&CancellationToken::default(), tasks, 2, &budget).unwrap();
    assert_eq!(budget.active_workers(), 0);
    let prepared: Vec<_> = results
        .into_iter()
        .map(|(id, mutations)| {
            (
                id,
                owner
                    .prepare_optimistic_write(1, snapshot, vec![], mutations)
                    .unwrap(),
            )
        })
        .collect();
    let mut results = prepared
        .into_iter()
        .map(|(id, write)| owner.commit_optimistic_idempotent(id, write));
    assert_eq!(results.next().unwrap().unwrap().sequence(), 1);
    assert_eq!(results.next().unwrap().unwrap().sequence(), 2);
    assert!(matches!(
        results.next().unwrap(),
        Err(WalBackedError::Tablet(
            TabletError::OptimisticConflict { .. }
        ))
    ));
    drop(results);
    drop(owner);
    let reopened = fixture.open();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_latest(b"a"), Some(&[0][..]));
    assert_eq!(reopened.read_latest(b"b"), Some(&[1][..]));
}

#[test]
fn cancelled_preparation_never_reaches_commit() {
    let fixture = Fixture::new();
    let owner = fixture.open();
    let token = CancellationToken::default();
    token.cancel();
    let budget = SharedTaskBudget::new(1).unwrap();
    let tasks: Vec<ScopedTask<'_, (), ()>> = vec![Box::new(|_| panic!("cancelled work started"))];
    assert_eq!(
        run_scoped_tasks_with_budget(&token, tasks, 1, &budget),
        Err(ScopedTaskFailure::Cancelled)
    );
    assert_eq!(budget.active_workers(), 0);
    assert_eq!(owner.current_sequence(), 0);
    drop(owner);
    assert_eq!(fixture.open().current_sequence(), 0);
}
