//! A Raft-compatible *apply* contract for the NuDB ownership control plane.
//!
//! These inputs are already committed by an external consensus system. The
//! state machine must not interpret a local proposal as a committed log entry.
use nulang::database::ownership::{
    CommittedOwnershipEntry, NodeId, OwnershipCommand, OwnershipError,
    OwnershipStateMachine,
};
use nulang::database::tablet::TabletId;

fn node(value: u64) -> NodeId {
    NodeId::new(value).unwrap()
}

fn tablet(value: u64) -> TabletId {
    TabletId::new(value).unwrap()
}

fn bootstrap(index: u64, term: u64, tablet_id: u64, owner: u64, epoch: u64) -> CommittedOwnershipEntry {
    CommittedOwnershipEntry {
        index,
        term,
        command: OwnershipCommand::Bootstrap {
            tablet: tablet(tablet_id),
            owner: node(owner),
            epoch,
        },
    }
}

fn transfer(
    index: u64,
    term: u64,
    tablet_id: u64,
    from_owner: u64,
    expected_epoch: u64,
    to_owner: u64,
    next_epoch: u64,
) -> CommittedOwnershipEntry {
    CommittedOwnershipEntry {
        index,
        term,
        command: OwnershipCommand::Transfer {
            tablet: tablet(tablet_id),
            expected_owner: node(from_owner),
            expected_epoch,
            next_owner: node(to_owner),
            next_epoch,
        },
    }
}

#[test]
fn committed_handoff_rejects_stale_leader_and_aba_owner() {
    let mut state = OwnershipStateMachine::new();
    state.apply_committed(bootstrap(1, 4, 91, 11, 7)).unwrap();
    state.authorize_writer(tablet(91), node(11), 7).unwrap();

    let decision = state
        .apply_committed(transfer(2, 4, 91, 11, 7, 22, 9))
        .unwrap();
    assert_eq!(decision.owner, node(22));
    assert_eq!(decision.epoch, 9);
    assert_eq!(decision.committed_index, 2);
    assert_eq!(
        state.authorize_writer(tablet(91), node(11), 7),
        Err(OwnershipError::StaleEpoch {
            committed: 9,
            presented: 7,
        })
    );
    assert_eq!(
        state.authorize_writer(tablet(91), node(11), 9),
        Err(OwnershipError::WrongOwner {
            committed: node(22),
            presented: node(11),
        })
    );

    // Owner 11 can return, but never with its earlier token or epoch.
    state.apply_committed(transfer(3, 5, 91, 22, 9, 11, 10)).unwrap();
    state.authorize_writer(tablet(91), node(11), 10).unwrap();
    assert!(state.authorize_writer(tablet(91), node(11), 7).is_err());
}

#[test]
fn uncommitted_or_out_of_order_entries_never_mutate_authority() {
    let mut state = OwnershipStateMachine::new();
    let initial = bootstrap(1, 2, 91, 11, 7);
    let receipt = state.apply_committed(initial.clone()).unwrap();
    assert_eq!(state.apply_committed(initial).unwrap(), receipt);

    let snapshot = state.clone();
    assert!(matches!(
        state.apply_committed(transfer(3, 3, 91, 11, 7, 22, 8)),
        Err(OwnershipError::LogGap { .. })
    ));
    assert!(matches!(
        state.apply_committed(transfer(2, 1, 91, 11, 7, 22, 8)),
        Err(OwnershipError::TermRegression { .. })
    ));
    assert_eq!(state, snapshot);

    let accepted = transfer(2, 3, 91, 11, 7, 22, 8);
    state.apply_committed(accepted.clone()).unwrap();
    let after = state.clone();

    assert_eq!(
        state.apply_committed(transfer(2, 3, 91, 11, 7, 33, 8)),
        Err(OwnershipError::ConflictingCommittedEntry { index: 2 })
    );
    assert_eq!(state.apply_committed(accepted).unwrap().epoch, 8);
    assert_eq!(state, after);
}

#[test]
fn ownership_transfer_must_match_previous_owner_and_epoch() {
    let mut state = OwnershipStateMachine::new();
    state.apply_committed(bootstrap(1, 3, 91, 11, 7)).unwrap();
    let before = state.clone();

    assert!(matches!(
        state.apply_committed(transfer(2, 3, 91, 22, 7, 33, 9)),
        Err(OwnershipError::WrongOwner { .. })
    ));
    assert!(matches!(
        state.apply_committed(transfer(2, 3, 91, 11, 6, 22, 9)),
        Err(OwnershipError::StaleEpoch { .. })
    ));
    assert!(matches!(
        state.apply_committed(transfer(2, 3, 91, 11, 7, 22, 7)),
        Err(OwnershipError::EpochNotAdvanced { .. })
    ));
    assert_eq!(state, before);
}

#[test]
fn deterministic_replay_reconstructs_the_same_authority() {
    let entries = vec![
        bootstrap(1, 3, 91, 11, 7),
        bootstrap(2, 3, 92, 33, 1),
        transfer(3, 4, 91, 11, 7, 22, 8),
        transfer(4, 4, 92, 33, 1, 33, 2),
    ];
    let mut original = OwnershipStateMachine::new();
    let mut recovered = OwnershipStateMachine::new();
    for entry in &entries {
        original.apply_committed(entry.clone()).unwrap();
    }
    for entry in &entries {
        recovered.apply_committed(entry.clone()).unwrap();
    }
    assert_eq!(recovered, original);
    assert_eq!(recovered.applied_index(), 4);
    assert_eq!(recovered.applied_term(), 4);
    recovered.authorize_writer(tablet(91), node(22), 8).unwrap();
    recovered.authorize_writer(tablet(92), node(33), 2).unwrap();
    assert_eq!(
        recovered.authorize_writer(tablet(91), node(22), 9),
        Err(OwnershipError::FutureEpoch { committed: 8, presented: 9 })
    );
}

#[test]
fn bootstrap_cannot_steal_existing_tablet_or_use_zero_epoch() {
    let mut state = OwnershipStateMachine::new();
    assert!(matches!(
        state.apply_committed(bootstrap(1, 1, 91, 11, 0)),
        Err(OwnershipError::InvalidEpoch)
    ));
    assert_eq!(state.applied_index(), 0);
    state.apply_committed(bootstrap(1, 1, 91, 11, 7)).unwrap();
    assert!(matches!(
        state.apply_committed(bootstrap(2, 1, 91, 22, 8)),
        Err(OwnershipError::AlreadyBootstrapped { .. })
    ));
    assert_eq!(state.applied_index(), 1);
}
