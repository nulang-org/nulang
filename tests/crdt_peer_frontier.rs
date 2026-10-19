use nulang::crdt_peer_sync::PeerCrdtFrontiers;
use nulang::runtime::{CrdtEntry, CrdtManager};

#[test]
fn peer_frontiers_advance_only_after_that_peer_acknowledges() {
    let mut source = CrdtManager::new(1);
    let mut peer_two = CrdtManager::new(2);
    let mut peer_three = CrdtManager::new(3);
    let mut frontiers = PeerCrdtFrontiers::new();

    let (id, mut counter) = source.create_gcounter();
    counter.increment_by(7);
    source.entries.insert(id, CrdtEntry::GCounter(counter));

    let first_two = frontiers
        .generate(&source, 2)
        .expect("peer two needs the initial full state");
    let first_three = frontiers
        .generate(&source, 3)
        .expect("peer three independently needs the initial full state");

    assert_eq!(first_two.ops.len(), 1);
    assert_eq!(first_three.ops.len(), 1);
    assert!(!first_two.ops[0].is_delta);
    assert!(!first_three.ops[0].is_delta);

    for op in first_two.ops.iter().cloned() {
        peer_two.apply_delta_op(op);
    }
    assert!(frontiers.acknowledge(2, first_two.batch_id));

    source.get_gcounter_mut(id).unwrap().increment_by(3);

    let second_two = frontiers
        .generate(&source, 2)
        .expect("peer two needs only the post-ack change");
    assert!(second_two.ops[0].is_delta);

    let second_three = frontiers
        .generate(&source, 3)
        .expect("peer three never acknowledged a base");
    assert!(
        !second_three.ops[0].is_delta,
        "one peer's acknowledgement must not advance another peer's frontier"
    );

    // Simulate loss of peer two's first delta attempt: no acknowledgement.
    let retry_two = frontiers
        .generate(&source, 2)
        .expect("an unacknowledged delta must remain sendable");
    assert!(retry_two.ops[0].is_delta);
    for op in retry_two.ops.iter().cloned() {
        peer_two.apply_delta_op(op);
    }
    assert_eq!(peer_two.get_gcounter_mut(id).unwrap().value(), 10);
    assert!(frontiers.acknowledge(2, retry_two.batch_id));
    assert!(frontiers.generate(&source, 2).is_none());

    // A late acknowledgement for the older lost batch must not move the
    // receiver frontier backwards after the newer retry was acknowledged.
    assert!(!frontiers.acknowledge(2, second_two.batch_id));
    assert!(frontiers.generate(&source, 2).is_none());

    // Peer three can still accept and acknowledge its independently generated
    // full-state batch without affecting peer two.
    for op in second_three.ops.iter().cloned() {
        peer_three.apply_delta_op(op);
    }
    assert_eq!(peer_three.get_gcounter_mut(id).unwrap().value(), 10);
    assert!(frontiers.acknowledge(3, second_three.batch_id));
    assert!(frontiers.generate(&source, 3).is_none());
}
