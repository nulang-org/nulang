//! Opt-in, non-production NuDB WAL growth and checkpoint recovery profile.
//!
//! Run with:
//! cargo test --locked --offline --no-default-features --test nudb_wal_capacity_profile -- --ignored --nocapture
//!
//! Measures actual fsync-backed commits on the test host. Timing is diagnostic,
//! not a regression threshold; it is sensitive to filesystem and shared-host load.

use std::fs;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

const WRITES: usize = 256;
const VALUE_BYTES: usize = 1024;
const HISTORICAL_SNAPSHOT: u64 = 64;

struct IsolatedDirectory(PathBuf);

impl Drop for IsolatedDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(916).expect("id"),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).expect("range"),
        1,
    )
    .expect("tablet")
}

#[test]
#[ignore = "opt-in disk/fsync benchmark; do not run in ordinary CI"]
fn profile_wal_growth_checkpoint_reclaim_and_restart() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock")
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("nudb-capacity-{}-{nonce}", std::process::id()));
    fs::create_dir(&directory).expect("create private test directory");
    let _cleanup = IsolatedDirectory(directory.clone());
    let path = directory.join("capacity.wal");

    let mut tablet = WalBackedTablet::open(descriptor(), &path).expect("open tablet");
    let start = Instant::now();
    let mut durations_us = Vec::with_capacity(WRITES);
    let mut previous_bytes = fs::metadata(&path).expect("initial WAL").len();

    for sequence in 1..=WRITES {
        let mut value = vec![0x5a; VALUE_BYTES];
        value[..8].copy_from_slice(&(sequence as u64).to_le_bytes());
        let write = tablet
            .prepare_write(
                1,
                (sequence - 1) as u64,
                vec![TabletMutation::Put {
                    key: b"key".to_vec(),
                    value,
                }],
            )
            .expect("prepare write");
        let begin = Instant::now();
        assert_eq!(
            tablet.commit(write).expect("durable commit"),
            sequence as u64
        );
        durations_us.push(begin.elapsed().as_micros());
        let wal_bytes = fs::metadata(&path).expect("appended WAL").len();
        assert!(
            wal_bytes > previous_bytes,
            "each commit appends a durable WAL frame"
        );
        previous_bytes = wal_bytes;
    }
    let commit_elapsed = start.elapsed();
    durations_us.sort_unstable();
    let p50 = durations_us[WRITES / 2];
    let p99 = durations_us[(WRITES * 99).div_ceil(100) - 1];
    let wal_before_checkpoint = previous_bytes;

    let checkpoint_begin = Instant::now();
    tablet
        .publish_checkpoint()
        .expect("durable checkpoint publication");
    let checkpoint_ms = checkpoint_begin.elapsed().as_millis();
    drop(tablet);

    let restart_begin = Instant::now();
    let mut restored =
        WalBackedTablet::open(descriptor(), &path).expect("checkpoint + WAL recovery");
    let restart_ms = restart_begin.elapsed().as_millis();
    assert_eq!(restored.current_sequence(), WRITES as u64);
    assert_eq!(
        &restored
            .read_at(b"key", HISTORICAL_SNAPSHOT)
            .expect("historic read")
            .expect("value")[..8],
        &HISTORICAL_SNAPSHOT.to_le_bytes()
    );
    assert_eq!(
        &restored.read_latest(b"key").expect("latest")[..8],
        &(WRITES as u64).to_le_bytes()
    );

    let reclaim_begin = Instant::now();
    restored.checkpoint().expect("unmanaged full-tail reclaim");
    let reclaim_ms = reclaim_begin.elapsed().as_millis();
    let wal_after_reclaim = fs::metadata(&path).expect("reclaimed WAL").len();
    assert!(
        wal_after_reclaim < wal_before_checkpoint,
        "checkpoint must reclaim WAL"
    );
    drop(restored);

    let reopened = WalBackedTablet::open(descriptor(), &path).expect("post-reclaim recovery");
    assert_eq!(reopened.current_sequence(), WRITES as u64);
    assert_eq!(
        &reopened
            .read_at(b"key", HISTORICAL_SNAPSHOT)
            .expect("historical read after reclaim")
            .expect("value")[..8],
        &HISTORICAL_SNAPSHOT.to_le_bytes()
    );

    println!(
        "NUDB_CAPACITY_PROFILE writes={WRITES} payload_bytes={VALUE_BYTES} wal_before_bytes={wal_before_checkpoint} wal_after_bytes={wal_after_reclaim} p50_commit_us={p50} p99_commit_us={p99} commit_total_ms={} checkpoint_ms={checkpoint_ms} restart_ms={restart_ms} reclaim_ms={reclaim_ms}",
        commit_elapsed.as_millis()
    );
}
