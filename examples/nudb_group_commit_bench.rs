//! Standalone NuDB group-commit microbenchmark, intentionally dependency-free.
//!
//! Run after PR #1736 → #1739 → #1742 have landed:
//!   cargo run --release --no-default-features --example nudb_group_commit_bench -- 4096
//!
//! Creates a fresh WAL for each batch size. Results are workload/hardware
//! dependent; this is a comparison harness, not a performance guarantee.
//! "group_p99" measures completion of a *whole group*, not per-transaction
//! latency, and counts no queueing delays. The baseline uses the public
//! single-transaction API, while grouped runs use commit_batch_idempotent.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};

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

fn mutation(sequence: usize) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: b"k".to_vec(),
        value: format!("value-{sequence:016x}").into_bytes(),
    }]
}

fn percentile(sorted: &[Duration], numerator: usize) -> f64 {
    let rank = sorted.len().saturating_mul(numerator).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)].as_secs_f64() * 1_000_000.0
}

fn run(dir: PathBuf, transactions: usize, batch_size: usize) {
    fs::create_dir_all(&dir).expect("create isolated benchmark directory");
    let wal = dir.join("tablet.wal");
    let mut tablet = WalBackedTablet::open(descriptor(), &wal).expect("open fresh tablet");
    let mut group_latencies = Vec::with_capacity(transactions.div_ceil(batch_size));

    let run_start = Instant::now();
    for first in (0..transactions).step_by(batch_size) {
        let last = (first + batch_size).min(transactions);
        let batch = (first..last)
            .map(|sequence| (identity(sequence + 1), mutation(sequence + 1)))
            .collect::<Vec<_>>();

        let start = Instant::now();
        if batch_size == 1 {
            let (id, mutations) = batch.into_iter().next().unwrap();
            tablet
                .commit_idempotent(id, mutations)
                .expect("single transaction commit");
        } else {
            tablet
                .commit_batch_idempotent(batch)
                .expect("bounded grouped commit");
        }
        group_latencies.push(start.elapsed());
    }
    let elapsed = run_start.elapsed();
    assert_eq!(tablet.current_sequence(), transactions as u64);
    drop(tablet);

    let recovery_start = Instant::now();
    let reopened = WalBackedTablet::open(descriptor(), &wal).expect("reopen committed WAL");
    let recovery_elapsed = recovery_start.elapsed();
    assert_eq!(reopened.current_sequence(), transactions as u64);
    assert_eq!(
        reopened.read_latest(b"k"),
        Some(format!("value-{transactions:016x}").as_bytes())
    );
    drop(reopened);

    group_latencies.sort_unstable();
    println!(
        "{batch_size},{transactions},{:.3},{:.1},{:.1},{:.1},{:.1},{:.3}",
        elapsed.as_secs_f64() * 1000.0,
        transactions as f64 / elapsed.as_secs_f64(),
        percentile(&group_latencies, 50),
        percentile(&group_latencies, 95),
        percentile(&group_latencies, 99),
        recovery_elapsed.as_secs_f64() * 1000.0,
    );
    fs::remove_dir_all(dir).expect("remove benchmark fixtures");
}

fn main() {
    let transactions: usize = std::env::args()
        .nth(1)
        .map(|value| value.parse().expect("transaction count must be integer"))
        .unwrap_or(4096);
    assert!(
        transactions >= 64 && transactions <= 100_000,
        "transaction count must be between 64 and 100000"
    );

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("valid clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "nulang_nudb_bench_{}_{}",
        std::process::id(),
        unique
    ));
    println!("batch,transactions,elapsed_ms,transactions_per_sec,group_p50_us,group_p95_us,group_p99_us,recovery_ms");
    for size in [1, 4, 16, 64] {
        run(root.join(format!("batch_{size}")), transactions, size);
    }
    fs::remove_dir_all(root).expect("remove benchmark directory");
}
