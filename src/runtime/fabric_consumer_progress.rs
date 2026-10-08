//! Durable, staged consumer metadata journal.
//!
//! Intentionally not yet wired to network authentication or the public
//! Fabric ACK APIs. This module is a storage/state-machine building block.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nulang-fabric-consumer-progress-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn policy(replicas: Vec<u64>) -> FabricConsumerProgressPolicy {
        FabricConsumerProgressPolicy {
            epoch: 1,
            leader: 10,
            replicas,
        }
    }

    fn change(index: u64, previous: u64, cursor: u64) -> FabricConsumerProgressChange {
        FabricConsumerProgressChange {
            stream: "orders".to_string(),
            partition: 0,
            consumer: "billing".to_string(),
            epoch: 1,
            generation: 1,
            metadata_sequence: index,
            previous_metadata_sequence: previous,
            committed_cursor: cursor,
            acked_gaps: Vec::new(),
        }
    }

    #[test]
    fn pending_metadata_is_not_visible_as_committed_after_restart() {
        let root = temp_root("pending");
        let group = policy(vec![10, 11, 12]);
        {
            let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
            journal.prepare(change(1, 0, 2), group.clone(), 5).unwrap();
            assert_eq!(journal.pending_sequence(), Some(1));
            assert_eq!(journal.committed_cursor("orders", 0, "billing"), 0);
            assert!(journal.commit_with_acknowledgers(1, &[10]).is_err());
            assert!(journal.commit_with_acknowledgers(1, &[10, 10]).is_err());
        }

        let mut reopened = FileFabricConsumerProgressJournal::open(&root).unwrap();
        assert_eq!(reopened.pending_sequence(), Some(1));
        assert_eq!(reopened.committed_cursor("orders", 0, "billing"), 0);
        reopened.commit_with_acknowledgers(1, &[10, 11]).unwrap();
        assert_eq!(reopened.pending_sequence(), None);
        assert_eq!(reopened.committed_cursor("orders", 0, "billing"), 2);

        let after_restart = FileFabricConsumerProgressJournal::open(&root).unwrap();
        assert_eq!(after_restart.committed_cursor("orders", 0, "billing"), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn quorum_certificate_cannot_use_nonmembers_or_skip_leader() {
        let root = temp_root("certificate");
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 1), policy(vec![10, 11, 12]), 1).unwrap();
        for invalid in [&[11, 12][..], &[10, 99][..], &[10][..], &[10, 10][..]] {
            assert!(journal.commit_with_acknowledgers(1, invalid).is_err());
            assert_eq!(journal.committed_cursor("orders", 0, "billing"), 0);
        }
        journal.commit_with_acknowledgers(1, &[10, 12]).unwrap();
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn metadata_sequence_and_consumer_cursor_never_go_backwards() {
        let root = temp_root("monotonic");
        let group = policy(vec![10, 11, 12]);
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 3), group.clone(), 5).unwrap();
        // The exact duplicate is idempotent; a different uncommitted proposal
        // must not replace the original pending record.
        journal.prepare(change(1, 0, 3), group.clone(), 5).unwrap();
        assert!(journal.prepare(change(1, 0, 4), group.clone(), 5).is_err());
        journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();

        assert!(journal.prepare(change(3, 1, 4), group.clone(), 5).is_err());
        assert!(journal.prepare(change(2, 0, 4), group.clone(), 5).is_err());
        assert!(journal.prepare(change(2, 1, 2), group.clone(), 5).is_err());
        journal.prepare(change(2, 1, 4), group.clone(), 5).unwrap();
        journal.commit_with_acknowledgers(2, &[10, 12]).unwrap();

        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 4);
        assert_eq!(journal.last_committed_metadata_sequence(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gaps_must_be_sorted_bounded_and_preserved_until_consumed() {
        let root = temp_root("gaps");
        let group = policy(vec![10, 11]);
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        let mut first = change(1, 0, 1);
        first.acked_gaps = vec![3, 5];
        journal.prepare(first, group.clone(), 5).unwrap();
        journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();

        let mut lost_gap = change(2, 1, 2);
        lost_gap.acked_gaps = vec![5];
        assert!(journal.prepare(lost_gap, group.clone(), 5).is_err());

        let mut invalid_order = change(2, 1, 2);
        invalid_order.acked_gaps = vec![5, 3];
        assert!(journal.prepare(invalid_order, group.clone(), 5).is_err());

        let mut past_tail = change(2, 1, 2);
        past_tail.acked_gaps = vec![3, 6];
        assert!(journal.prepare(past_tail, group.clone(), 5).is_err());

        let mut second = change(2, 1, 3);
        second.acked_gaps = vec![5];
        journal.prepare(second, group, 5).unwrap();
        journal.commit_with_acknowledgers(2, &[10, 11]).unwrap();
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 3);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_epochs_and_policy_membership_fail_before_appending() {
        let root = temp_root("epochs");
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        let mut bad_epoch = change(1, 0, 1);
        bad_epoch.epoch = 2;
        assert!(journal.prepare(bad_epoch, policy(vec![10, 11]), 1).is_err());
        assert!(journal.prepare(change(1, 0, 1), policy(vec![10, 10]), 1).is_err());
        assert!(journal.prepare(change(1, 0, 1), policy(vec![11, 12]), 1).is_err());
        assert!(journal.prepare(change(1, 0, 2), policy(vec![10, 11]), 1).is_err());
        assert_eq!(journal.pending_sequence(), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampering_and_partial_journal_tail_fail_closed_on_open() {
        let root = temp_root("corruption");
        let group = policy(vec![10, 11]);
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 1), group, 1).unwrap();
        drop(journal);
        let path = root.join("consumer_progress.log");
        let mut bytes = fs::read(&path).unwrap();
        let pos = bytes.iter().position(|byte| *byte == b'1').unwrap();
        bytes[pos] = b'9';
        fs::write(&path, &bytes).unwrap();
        assert!(FileFabricConsumerProgressJournal::open(&root).is_err());

        fs::write(&path, b"{\"version\":1").unwrap();
        assert!(FileFabricConsumerProgressJournal::open(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_replica_metadata_requires_both_durable_votes() {
        let root = temp_root("rf2");
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 1), policy(vec![10, 11]), 1).unwrap();
        assert!(journal.commit_with_acknowledgers(1, &[10]).is_err());
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 0);
        journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 1);
        let _ = fs::remove_dir_all(root);
    }
}
