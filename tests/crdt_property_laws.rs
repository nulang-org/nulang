//! Seeded CRDT semilattice and delta-replication properties.
//!
//! All checks reconstruct their replicas from generated operations, so DST
//! shrinking can rerun each candidate without reusing mutable CRDT state.
//! Compare replicated payloads rather than node-local identity metadata.

use nulang::dst::property::{check_trace_property, TracePropertyConfig};
use nulang::dst::DeterministicRng;
use nulang::runtime::crdt::{Crdt, GCounter, GSet, PNCounter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CounterOp {
    Increment { replica: u8, amount: u64 },
    Decrement { replica: u8, amount: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SetOp {
    replica: u8,
    value: String,
}

fn generate_counter_ops(rng: &mut DeterministicRng, allow_decrements: bool) -> Vec<CounterOp> {
    let len = 1 + rng.next() % 36;
    (0..len)
        .map(|_| {
            let replica = (rng.next() % 3) as u8;
            let amount = 1 + rng.next() % 9;
            if allow_decrements && rng.next() % 2 == 0 {
                CounterOp::Decrement { replica, amount }
            } else {
                CounterOp::Increment { replica, amount }
            }
        })
        .collect()
}

fn generate_set_ops(rng: &mut DeterministicRng) -> Vec<SetOp> {
    let len = 1 + rng.next() % 36;
    (0..len)
        .map(|_| SetOp {
            replica: (rng.next() % 3) as u8,
            value: format!("item-{}", rng.next() % 12),
        })
        .collect()
}

fn g_counter_laws(operations: &[CounterOp]) -> bool {
    let mut replicas = [
        GCounter::new(1),
        GCounter::new(2),
        GCounter::new(3),
    ];
    for operation in operations {
        let CounterOp::Increment { replica, amount } = *operation else {
            // Shrinker may delete operations, but must not invent invalid ones.
            return false;
        };
        replicas[replica as usize].increment_by(amount);
    }
    let [a, b, c] = replicas;

    let mut ab = a.clone();
    ab.merge(&b);
    let mut ba = b.clone();
    ba.merge(&a);
    if ab.counts != ba.counts || ab.value() != ba.value() {
        return false;
    }

    let mut left = ab.clone();
    left.merge(&c);
    let mut bc = b.clone();
    bc.merge(&c);
    let mut right = a.clone();
    right.merge(&bc);
    if left.counts != right.counts || left.value() != right.value() {
        return false;
    }

    let mut idem = a.clone();
    idem.merge(&a);
    if idem.counts != a.counts || GCounter::from_bytes(&a.to_bytes()) != Some(a.clone()) {
        return false;
    }

    // Delta equivalence requires a causal base: current = base join b.
    let base = a.clone();
    if base.delta_since(&base).is_some() {
        return false;
    }
    let current = ab.clone();
    // The receiver contains the base plus unrelated concurrent updates.
    let mut receiver = base.clone();
    receiver.merge(&c);
    let mut via_full = receiver.clone();
    via_full.merge(&current);
    let mut via_delta = receiver;
    match current.delta_since(&base) {
        Some(delta) => {
            via_delta.merge(&delta);
            let once = via_delta.counts.clone();
            via_delta.merge(&delta);
            if via_delta.counts != once || GCounter::from_bytes(&delta.to_bytes()) != Some(delta) {
                return false;
            }
        }
        None => {}
    }

    via_full.counts == via_delta.counts && via_full.value() == via_delta.value()
}

fn pn_counter_laws(operations: &[CounterOp]) -> bool {
    let mut replicas = [
        PNCounter::new(1),
        PNCounter::new(2),
        PNCounter::new(3),
    ];
    for operation in operations {
        match *operation {
            CounterOp::Increment { replica, amount } => {
                replicas[replica as usize].increment_by(amount);
            }
            CounterOp::Decrement { replica, amount } => {
                replicas[replica as usize].decrement_by(amount);
            }
        }
    }
    let [a, b, c] = replicas;

    // PN state is a pair of GCounter joins. Node-local IDs are not compared.
    let same_state = |x: &PNCounter, y: &PNCounter| {
        x.increments.counts == y.increments.counts
            && x.decrements.counts == y.decrements.counts
            && x.value() == y.value()
    };
    let mut ab = a.clone();
    ab.merge(&b);
    let mut ba = b.clone();
    ba.merge(&a);
    if !same_state(&ab, &ba) {
        return false;
    }

    let mut left = ab.clone();
    left.merge(&c);
    let mut bc = b.clone();
    bc.merge(&c);
    let mut right = a.clone();
    right.merge(&bc);
    if !same_state(&left, &right) {
        return false;
    }

    let mut idem = a.clone();
    idem.merge(&a);
    if !same_state(&idem, &a) || PNCounter::from_bytes(&a.to_bytes()) != Some(a.clone()) {
        return false;
    }

    let base = a.clone();
    if base.delta_since(&base).is_some() {
        return false;
    }
    let current = ab;
    let mut receiver = base.clone();
    receiver.merge(&c);
    let mut full = receiver.clone();
    full.merge(&current);
    let mut delta_applied = receiver;
    if let Some(delta) = current.delta_since(&base) {
        delta_applied.merge(&delta);
        let once = delta_applied.clone();
        delta_applied.merge(&delta);
        if !same_state(&once, &delta_applied)
            || PNCounter::from_bytes(&delta.to_bytes()) != Some(delta)
        {
            return false;
        }
    }

    same_state(&full, &delta_applied)
}

fn g_set_laws(operations: &[SetOp]) -> bool {
    let mut replicas = [
        GSet::<String>::new(),
        GSet::<String>::new(),
        GSet::<String>::new(),
    ];
    for operation in operations {
        replicas[operation.replica as usize].insert(operation.value.clone());
    }
    let [a, b, c] = replicas;

    let mut ab = a.clone();
    ab.merge(&b);
    let mut ba = b.clone();
    ba.merge(&a);
    if ab != ba || ab.value() != ba.value() {
        return false;
    }

    let mut left = ab.clone();
    left.merge(&c);
    let mut bc = b.clone();
    bc.merge(&c);
    let mut right = a.clone();
    right.merge(&bc);
    if left != right {
        return false;
    }

    let mut idem = a.clone();
    idem.merge(&a);
    if idem != a || GSet::<String>::from_bytes(&a.to_bytes()) != Some(a.clone()) {
        return false;
    }

    let base = a.clone();
    if base.delta_since(&base).is_some() {
        return false;
    }
    let current = ab;
    let mut receiver = base.clone();
    receiver.merge(&c);
    let mut full = receiver.clone();
    full.merge(&current);
    let mut delta_applied = receiver;
    if let Some(delta) = current.delta_since(&base) {
        delta_applied.merge(&delta);
        let once = delta_applied.clone();
        delta_applied.merge(&delta);
        if delta_applied != once || GSet::<String>::from_bytes(&delta.to_bytes()) != Some(delta) {
            return false;
        }
    }

    full == delta_applied
}

#[test]
fn generated_gcounter_join_and_delta_laws() {
    let result = check_trace_property(
        TracePropertyConfig {
            seed: 0x6c0,
            cases: 128,
            max_shrink_replays: 300,
        },
        |rng| generate_counter_ops(rng, false),
        g_counter_laws,
    );
    assert_eq!(result, Ok(128), "gcounter property: {result:?}");
}

#[test]
fn generated_pncounter_join_and_delta_laws() {
    let result = check_trace_property(
        TracePropertyConfig {
            seed: 0x9c0,
            cases: 128,
            max_shrink_replays: 300,
        },
        |rng| generate_counter_ops(rng, true),
        pn_counter_laws,
    );
    assert_eq!(result, Ok(128), "pncounter property: {result:?}");
}

#[test]
fn generated_gset_join_and_delta_laws() {
    let result = check_trace_property(
        TracePropertyConfig {
            seed: 0x651,
            cases: 128,
            max_shrink_replays: 300,
        },
        generate_set_ops,
        g_set_laws,
    );
    assert_eq!(result, Ok(128), "gset property: {result:?}");
}
