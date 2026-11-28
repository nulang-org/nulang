//! Compare equivalent disjoint-key workloads across durable single and grouped
//! commits, with and without point-key optimistic validation.
//! cargo run --release --no-default-features --example nudb_optimistic_group_bench -- /existing/storage/dir 4096
//! Explicit storage directory: do not silently measure tmpfs. Fresh WAL per
//! mode/size; group latency includes preparation and commit but not queue wait.
//! Recovery timing includes reopening only, not the subsequent receipt checks.
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new(root: &Path) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = root.join(format!(
                "nudb-occ-bench-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create isolated fixture: {e}"),
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.0) {
            eprintln!("could not remove owned benchmark fixture {:?}: {e}", self.0);
            if !std::thread::panicking() {
                panic!("benchmark fixture cleanup failed");
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Single,
    Batch,
    OptimisticSingle,
    OptimisticBatch,
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(121).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn identity(sequence: usize) -> TransactionId {
    TransactionId::new((sequence as u128).to_le_bytes())
}

fn key(sequence: usize) -> Vec<u8> {
    format!("k{sequence:016x}").into_bytes()
}

fn value(sequence: usize) -> Vec<u8> {
    format!("value-{sequence:016x}").into_bytes()
}

fn mutation(sequence: usize) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: key(sequence),
        value: value(sequence),
    }]
}

struct Report {
    transactions: usize,
    groups: usize,
    elapsed: Duration,
    group_latencies: Vec<Duration>,
    recovery_elapsed: Duration,
}

fn percentile(sorted: &[Duration], numerator: usize) -> f64 {
    let rank = sorted.len().saturating_mul(numerator).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)].as_secs_f64() * 1_000_000.0
}

fn run(root: &Path, transactions: usize, batch_size: usize, mode: Mode) -> Report {
    assert!((1..=100_000).contains(&transactions));
    assert!((1..=64).contains(&batch_size));
    if matches!(mode, Mode::Single | Mode::OptimisticSingle) {
        assert_eq!(batch_size, 1);
    }
    let fixture = Fixture::new(root);
    let wal = fixture.0.join("tablet.wal");
    let mut tablet = WalBackedTablet::open(descriptor(), &wal).unwrap();
    let mut group_latencies = Vec::with_capacity(transactions.div_ceil(batch_size));
    let run_start = Instant::now();
    for first in (1..=transactions).step_by(batch_size) {
        let last = (first + batch_size - 1).min(transactions);
        let start = Instant::now();
        let receipts = match mode {
            Mode::Single => vec![tablet
                .commit_idempotent(identity(first), mutation(first))
                .unwrap()],
            Mode::Batch => tablet
                .commit_batch_idempotent(
                    (first..=last)
                        .map(|seq| (identity(seq), mutation(seq)))
                        .collect(),
                )
                .unwrap(),
            Mode::OptimisticSingle | Mode::OptimisticBatch => {
                let snapshot = tablet.current_sequence();
                let mut entries = (first..=last)
                    .map(|seq| {
                        (
                            identity(seq),
                            tablet
                                .prepare_optimistic_write(1, snapshot, vec![], mutation(seq))
                                .unwrap(),
                        )
                    })
                    .collect::<Vec<_>>();
                if matches!(mode, Mode::OptimisticSingle) {
                    let (id, write) = entries.pop().unwrap();
                    vec![tablet.commit_optimistic_idempotent(id, write).unwrap()]
                } else {
                    tablet.commit_optimistic_batch_idempotent(entries).unwrap()
                }
            }
        };
        group_latencies.push(start.elapsed());
        assert_eq!(receipts.len(), last - first + 1);
        for (seq, receipt) in (first..=last).zip(receipts) {
            assert_eq!(receipt.sequence(), seq as u64);
        }
    }
    let elapsed = run_start.elapsed();
    assert_eq!(tablet.current_sequence(), transactions as u64);
    drop(tablet);

    let recovery_start = Instant::now();
    let mut reopened = WalBackedTablet::open(descriptor(), &wal).unwrap();
    let recovery_elapsed = recovery_start.elapsed();
    assert_eq!(reopened.current_sequence(), transactions as u64);
    let wal_len = fs::metadata(&wal).unwrap().len();
    for seq in 1..=transactions {
        assert_eq!(reopened.read_latest(&key(seq)), Some(value(seq).as_slice()));
        assert_eq!(reopened.read_at(&key(seq), seq as u64 - 1).unwrap(), None);
        assert_eq!(
            reopened
                .commit_idempotent(identity(seq), mutation(seq))
                .unwrap()
                .sequence(),
            seq as u64
        );
    }
    assert_eq!(reopened.current_sequence(), transactions as u64);
    assert_eq!(fs::metadata(&wal).unwrap().len(), wal_len);
    drop(reopened);
    group_latencies.sort_unstable();
    Report {
        transactions,
        groups: group_latencies.len(),
        elapsed,
        group_latencies,
        recovery_elapsed,
    }
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(
        args.next()
            .expect("provide an existing benchmark storage directory"),
    );
    assert!(root.is_dir(), "storage directory must exist");
    let transactions = args
        .next()
        .map(|arg| {
            arg.to_str()
                .expect("UTF-8 transaction count")
                .parse::<usize>()
                .expect("integer transaction count")
        })
        .unwrap_or(4096);
    assert!(
        (64..=100_000).contains(&transactions),
        "transaction count must be in 64..=100000"
    );
    assert!(args.next().is_none(), "unexpected argument");
    println!("mode,batch,transactions,groups,elapsed_ms,transactions_per_sec,group_p50_us,group_p95_us,group_p99_us,recovery_ms");
    for (mode, sizes) in [
        (Mode::Single, &[1_usize][..]),
        (Mode::Batch, &[4, 16, 64][..]),
        (Mode::OptimisticSingle, &[1][..]),
        (Mode::OptimisticBatch, &[4, 16, 64][..]),
    ] {
        for &batch in sizes {
            let r = run(&root, transactions, batch, mode);
            println!(
                "{mode:?},{batch},{},{},{:.3},{:.1},{:.1},{:.1},{:.1},{:.3}",
                r.transactions,
                r.groups,
                r.elapsed.as_secs_f64() * 1000.0,
                r.transactions as f64 / r.elapsed.as_secs_f64(),
                percentile(&r.group_latencies, 50),
                percentile(&r.group_latencies, 95),
                percentile(&r.group_latencies, 99),
                r.recovery_elapsed.as_secs_f64() * 1000.0
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_modes_commit_same_disjoint_workload_and_recover_partial_group() {
        let root = Fixture::new(&std::env::temp_dir());
        for (mode, batch) in [
            (Mode::Single, 1),
            (Mode::Batch, 4),
            (Mode::OptimisticSingle, 1),
            (Mode::OptimisticBatch, 4),
        ] {
            let result = run(&root.0, 9, batch, mode);
            assert_eq!(result.transactions, 9);
            assert_eq!(result.groups, 9_usize.div_ceil(batch));
        }
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 0);
    }
}
