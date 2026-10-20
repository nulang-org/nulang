//! Trace shrinking exercised against real Runtime nodes and virtual clocks.
//! The predicate is an intentionally chosen, reproducible fault signature,
//! not evidence that the production runtime violates an invariant.

use super::cluster_dst::DeterministicCluster;
use crate::dst::property::{check_trace_property, TracePropertyConfig};
use crate::dst::shrink::{minimize_failing_trace, MinimizationStatus};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Advance(u64),
    CrashNodeOne,
    RestartNodeOne,
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Replays *each* candidate from a fresh multi-node Runtime, rather than
/// reusing mutated state or evaluating a synthetic state-machine model.
/// Invalid command sequences are not considered matching failure signatures.
fn reproduces_crashed_node_clock_gap(ops: &[Fault]) -> bool {
    let mut cluster = DeterministicCluster::new(&[addr(9561), addr(9562)], 0x5eed);
    let mut crashed = false;

    for &op in ops {
        match op {
            Fault::Advance(rounds) => cluster.run_rounds(rounds),
            Fault::CrashNodeOne => {
                if crashed {
                    return false;
                }
                cluster.crash_node(1);
                crashed = true;
            }
            Fault::RestartNodeOne => {
                if !crashed {
                    return false;
                }
                cluster.restart_node(1);
                crashed = false;
            }
        }
    }

    // In this harness, a crashed node is not pumped, so the surviving
    // node's virtual clock advances beyond the stopped node's clock.
    let survivor = cluster.node(0).virtual_clock.as_ref().unwrap().elapsed();
    let stopped = cluster.node(1).virtual_clock.as_ref().unwrap().elapsed();
    crashed && survivor > stopped && cluster.limit_hits == 0
}

#[test]
fn shrink_replays_real_runtime_faults_to_one_minimal_trace() {
    let original = [
        Fault::Advance(2),
        Fault::CrashNodeOne,
        Fault::Advance(4),
        Fault::RestartNodeOne,
        Fault::Advance(1),
        Fault::CrashNodeOne,
        Fault::Advance(3),
    ];

    assert!(reproduces_crashed_node_clock_gap(&original));
    let reduced = minimize_failing_trace(&original, 200, reproduces_crashed_node_clock_gap);
    assert_eq!(reduced.status, MinimizationStatus::OneMinimal);
    assert_eq!(reduced.trace.len(), 2);
    assert!(matches!(reduced.trace[0], Fault::CrashNodeOne));
    assert!(matches!(reduced.trace[1], Fault::Advance(rounds) if rounds > 0));
    assert!(reproduces_crashed_node_clock_gap(&reduced.trace));

    // The same reduced trace is replayable without depending on the
    // original run's mutable state.
    let repeated = minimize_failing_trace(&original, 200, reproduces_crashed_node_clock_gap);
    assert_eq!(reduced, repeated);
}

#[test]
fn invalid_fault_sequences_do_not_count_as_reproduced_failures() {
    assert!(!reproduces_crashed_node_clock_gap(&[
        Fault::RestartNodeOne,
        Fault::Advance(2)
    ]));
    assert!(!reproduces_crashed_node_clock_gap(&[
        Fault::Advance(2),
        Fault::CrashNodeOne,
        Fault::RestartNodeOne,
    ]));
}

#[test]
fn seeded_fault_scripts_shrink_against_fresh_real_runtimes() {
    let result = check_trace_property(
        TracePropertyConfig {
            seed: 0x5eed,
            cases: 8,
            max_shrink_replays: 200,
        },
        |rng| {
            // The failure is intentionally constructed to test the harness,
            // not a claim that production Nulang violates an invariant.
            vec![
                Fault::Advance(1 + rng.next() % 3),
                Fault::CrashNodeOne,
                Fault::Advance(1 + rng.next() % 5),
                Fault::Advance(1 + rng.next() % 3),
            ]
        },
        |script| !reproduces_crashed_node_clock_gap(script),
    );
    let failure = result.expect_err("expected injected stopped-clock signature");

    assert_eq!(failure.case_index, 0);
    assert_eq!(failure.minimized.status, MinimizationStatus::OneMinimal);
    assert_eq!(failure.minimized.trace.len(), 2);
    assert!(matches!(failure.minimized.trace[0], Fault::CrashNodeOne));
    assert!(matches!(failure.minimized.trace[1], Fault::Advance(rounds) if rounds > 0));
    assert!(reproduces_crashed_node_clock_gap(&failure.minimized.trace));
}

#[test]
fn generated_live_cluster_rounds_preserve_virtual_clock_alignment() {
    let result = check_trace_property(
        TracePropertyConfig {
            seed: 0xc10c,
            cases: 8,
            max_shrink_replays: 100,
        },
        |rng| {
            (0..(1 + rng.next() % 4))
                .map(|_| Fault::Advance(1 + rng.next() % 3))
                .collect::<Vec<_>>()
        },
        |script| {
            // Replay each generated case against new real Runtime instances;
            // local virtual-time steps advance for all healthy nodes.
            let mut cluster = DeterministicCluster::new(&[addr(9563), addr(9564)], 0x5eed);
            let mut total_rounds = 0;
            for &operation in script {
                let Fault::Advance(rounds) = operation else {
                    return false;
                };
                cluster.run_rounds(rounds);
                total_rounds += rounds;
            }
            cluster.limit_hits == 0
                && cluster.round == total_rounds
                && cluster.node(0).virtual_clock.as_ref().unwrap().elapsed()
                    == cluster.node(1).virtual_clock.as_ref().unwrap().elapsed()
        },
    );
    assert_eq!(result, Ok(8));
}
