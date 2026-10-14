//! Fixed-term, fixed-membership quorum-durable ownership-log regressions.
//!
//! Separate journal files model independently durable voting replicas. They
//! are NOT a test of network partitions, elections or multi-host consensus.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::ownership::{NodeId, OwnershipCommand, OwnershipError};
use nulang::database::ownership_quorum::{FixedTermQuorumLog, QuorumLogError};
use nulang::database::tablet::TabletId;

static NEXT: AtomicU64 = AtomicU64::new(1);

fn paths() -> Vec<PathBuf> {
    let key = format!(
        "nudb_quorum_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    (0..3)
        .map(|voter| std::env::temp_dir().join(format!("{key}_{voter}.log")))
        .collect()
}
fn cleanup(paths: &[PathBuf]) {
    for path in paths {
        let _ = fs::remove_file(path);
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(PathBuf::from(lock));
    }
}
fn node(id: u64) -> NodeId {
    NodeId::new(id).unwrap()
}

fn tablet(id: u64) -> TabletId {
    TabletId::new(id).unwrap()
}
fn bootstrap() -> OwnershipCommand {
    OwnershipCommand::Bootstrap {
        tablet: tablet(18),
        owner: node(1),
        epoch: 7,
    }
}
fn handoff(
    expected_owner: u64,
    expected_epoch: u64,
    next_owner: u64,
    next_epoch: u64,
) -> OwnershipCommand {
    OwnershipCommand::Transfer {
        tablet: tablet(18),
        expected_owner: node(expected_owner),
        expected_epoch,
        next_owner: node(next_owner),
        next_epoch,
    }
}

#[test]
fn three_file_quorum_commits_and_recovers_identical_grants() {
    let paths = paths();
    cleanup(&paths);
    let mut log = FixedTermQuorumLog::bootstrap(&paths, 12).unwrap();
    let first = log.propose(bootstrap()).unwrap();
    assert_eq!(first.epoch, 7);
    assert_eq!(first.committed_index, 1);
    let second = log.propose(handoff(1, 7, 2, 9)).unwrap();
    assert_eq!(second.committed_index, 2);
    log.authority().authorize_writer(tablet(18), node(2), 9).unwrap();
    drop(log);

    let log = FixedTermQuorumLog::open(&paths, 12).unwrap();
    assert_eq!(log.authority().applied_index(), 2);
    assert_eq!(log.authority().grant(tablet(18)), Some(second));
    assert!(log.authority().authorize_writer(tablet(18), node(1), 7).is_err());
    drop(log);
    cleanup(&paths);
}

#[test]
fn fixed_term_cannot_silently_transition_to_new_leader_term() {
    let paths = paths();
    cleanup(&paths);
    let mut log = FixedTermQuorumLog::bootstrap(&paths, 4).unwrap();
    log.propose(bootstrap()).unwrap();
    drop(log);

    assert!(matches!(
        FixedTermQuorumLog::open(&paths, 5),
        Err(QuorumLogError::UnsupportedTermChange { durable: 4, requested: 5 })
    ));
    let log = FixedTermQuorumLog::open(&paths, 4).unwrap();
    assert_eq!(log.authority().applied_index(), 1);
    drop(log);
    cleanup(&paths);
}

#[test]
fn rejected_committed_command_still_advances_durable_quorum_log() {
    let paths = paths();
    cleanup(&paths);
    let mut log = FixedTermQuorumLog::bootstrap(&paths, 3).unwrap();
    log.propose(bootstrap()).unwrap();
    assert!(matches!(
        log.propose(handoff(2, 7, 3, 8)),
        Err(QuorumLogError::CommandRejected(OwnershipError::WrongOwner { .. }))
    ));
    assert_eq!(log.authority().applied_index(), 2);
    assert_eq!(log.authority().grant(tablet(18)).unwrap().owner, node(1));
    assert_eq!(log.propose(handoff(1, 7, 2, 8)).unwrap().committed_index, 3);
    drop(log);

    let reopened = FixedTermQuorumLog::open(&paths, 3).unwrap();
    assert_eq!(reopened.authority().applied_index(), 3);
    reopened.authority().authorize_writer(tablet(18), node(2), 8).unwrap();
    drop(reopened);
    cleanup(&paths);
}

#[test]
fn loss_of_one_voter_still_permits_quorum_recovery() {
    let paths = paths();
    cleanup(&paths);
    let mut log = FixedTermQuorumLog::bootstrap(&paths, 2).unwrap();
    log.propose(bootstrap()).unwrap();
    drop(log);

    // A disappeared replica must NOT be silently recreated as a fresh voter.
    fs::remove_file(&paths[2]).unwrap();
    let mut log = FixedTermQuorumLog::open(&paths, 2).unwrap();
    assert_eq!(log.authority().applied_index(), 1);
    assert_eq!(log.propose(handoff(1, 7, 2, 8)).unwrap().committed_index, 2);
    assert!(!paths[2].exists());
    drop(log);
    cleanup(&paths);
}

#[test]
fn no_quorum_cannot_recover_or_issue_ownership_grants() {
    let paths = paths();
    cleanup(&paths);
    let mut log = FixedTermQuorumLog::bootstrap(&paths, 2).unwrap();
    log.propose(bootstrap()).unwrap();
    drop(log);

    fs::remove_file(&paths[1]).unwrap();
    fs::remove_file(&paths[2]).unwrap();
    assert!(matches!(
        FixedTermQuorumLog::open(&paths, 2),
        Err(QuorumLogError::NotEnoughReplicas { available: 1, required: 2 })
    ));
    cleanup(&paths);
}

#[test]
fn duplicate_voter_paths_cannot_be_counted_as_two_votes() {
    let paths = paths();
    cleanup(&paths);
    let bogus = vec![paths[0].clone(), paths[0].clone(), paths[1].clone()];
    assert!(matches!(
        FixedTermQuorumLog::open(&bogus, 3),
        Err(QuorumLogError::DuplicateReplicaPath)
    ));
    cleanup(&paths);
}

#[test]
fn corrupted_minor_replica_does_not_override_majority() {
    use std::io::Write;
    let paths = paths();
    cleanup(&paths);
    let mut log = FixedTermQuorumLog::bootstrap(&paths, 4).unwrap();
    let committed = log.propose(bootstrap()).unwrap();
    drop(log);

    // A torn, unchecksummed frame makes the replica unavailable. It cannot
    // be counted as a valid acknowledgment or an alternate committed log.
    let mut damaged = fs::OpenOptions::new()
        .append(true)
        .open(&paths[2])
        .unwrap();
    damaged.write_all(&[0xff]).unwrap();
    damaged.sync_data().unwrap();
    drop(damaged);

    let mut reopened = FixedTermQuorumLog::open(&paths, 4).unwrap();
    assert_eq!(reopened.authority().grant(tablet(18)), Some(committed));
    let next = reopened.propose(handoff(1, 7, 2, 8)).unwrap();
    assert_eq!(next.committed_index, 2);
    drop(reopened);
    cleanup(&paths);
}

#[cfg(unix)]
#[test]
fn hardlinked_journal_files_must_not_be_counted_twice() {
    let paths = paths();
    cleanup(&paths);
    let log = FixedTermQuorumLog::bootstrap(&paths, 3).unwrap();
    drop(log);

    fs::remove_file(&paths[1]).unwrap();
    fs::hard_link(&paths[0], &paths[1]).unwrap();
    assert!(matches!(
        FixedTermQuorumLog::open(&paths, 3),
        Err(QuorumLogError::DuplicateReplicaPath)
    ));
    cleanup(&paths);
}

#[test]
fn all_missing_journals_cannot_implicitly_rebootstrap_an_existing_cluster() {
    let paths = paths();
    cleanup(&paths);
    let mut original = FixedTermQuorumLog::bootstrap(&paths, 3).unwrap();
    original.propose(bootstrap()).unwrap();
    drop(original);
    for path in &paths {
        fs::remove_file(path).unwrap();
    }
    // A normal restart must not mistake total replica loss for a fresh
    // cluster. Reinitialization requires an explicit operator action.
    assert!(matches!(
        FixedTermQuorumLog::open(&paths, 3),
        Err(QuorumLogError::NotEnoughReplicas {
            available: 0,
            required: 2
        })
    ));
    cleanup(&paths);
}

#[test]
fn explicit_bootstrap_rejects_partially_existing_membership() {
    let paths = paths();
    cleanup(&paths);
    let log = FixedTermQuorumLog::bootstrap(&paths, 3).unwrap();
    drop(log);
    fs::remove_file(&paths[2]).unwrap();
    assert!(matches!(
        FixedTermQuorumLog::bootstrap(&paths, 3),
        Err(QuorumLogError::AlreadyInitialized)
    ));
    cleanup(&paths);
}
