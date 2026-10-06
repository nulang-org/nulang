#![cfg(feature = "tcp")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread;
use std::time::{Duration, Instant};

use nulang::runtime::{Actor, ActorAddress, NodeId, Runtime, TlsConfig};
use nulang::vm::Value;

// REMOTE_RTT_STAGE_PROFILE_START
const HOP_TIMEOUT: Duration = Duration::from_secs(2);
const WARMUP_ROUNDTRIPS: u64 = 100;
const MEASURED_ROUNDTRIPS: u64 = 2_000;

#[derive(Default)]
struct StageBudget {
    send_ns: u128,
    process_network_ns: u128,
    run_scheduler_ns: u128,
    polls: u64,
    yield_count: u64,
}

impl StageBudget {
    fn add(&mut self, other: Self) {
        self.send_ns += other.send_ns;
        self.process_network_ns += other.process_network_ns;
        self.run_scheduler_ns += other.run_scheduler_ns;
        self.polls += other.polls;
        self.yield_count += other.yield_count;
    }
}

fn record_sequence(actor: &mut Actor, args: &[Value]) {
    if let Some(sequence) = args.first().and_then(Value::as_int) {
        actor.set_state_field("seen", Value::int(sequence));
    }
}

fn distributed_runtime() -> Runtime {
    let mut runtime = Runtime::new();
    runtime
        .enable_distribution(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            TlsConfig::PlaintextInsecure,
        )
        .expect("loopback distribution should bind");
    runtime
}

fn endpoint(runtime: &Runtime) -> (NodeId, SocketAddr) {
    let node = runtime
        .distributed
        .node_id
        .expect("distribution should assign a node id");
    let addr = runtime
        .distributed
        .transport
        .as_ref()
        .expect("distribution should own a transport")
        .listen_addr();
    (node, addr)
}

fn make_peer_healthy(runtime: &mut Runtime, peer: NodeId, addr: SocketAddr) {
    let cluster = runtime
        .distributed
        .cluster
        .as_mut()
        .expect("distribution should own cluster state");
    cluster.join_cluster_with_id(peer, addr);
    cluster.handle_heartbeat(peer, addr);
}

fn establish_loopback_link(
    left: &mut Runtime,
    right: &Runtime,
    right_node: NodeId,
    right_addr: SocketAddr,
) {
    left.distributed
        .transport
        .as_mut()
        .expect("left runtime should own a transport")
        .connect(right_node, right_addr)
        .expect("loopback TCP connection should establish before warm-up");

    let deadline = Instant::now() + HOP_TIMEOUT;
    loop {
        let left_connected = left
            .distributed
            .transport
            .as_ref()
            .expect("left runtime should retain its transport")
            .connection_count()
            > 0;
        let right_connected = right
            .distributed
            .transport
            .as_ref()
            .expect("right runtime should own a transport")
            .connection_count()
            > 0;
        if left_connected && right_connected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the explicit loopback connection to become visible on both runtimes"
        );
        thread::yield_now();
    }
}

fn spawn_probe(runtime: &mut Runtime) -> u64 {
    let actor_id = runtime.spawn_actor(Box::new(|| {
        vec![("seen".to_string(), Value::int(0))]
    }));
    runtime
        .actors
        .get_mut(&actor_id)
        .expect("spawned actor should exist")
        .register_behavior("record", record_sequence);
    runtime.run_scheduler();
    actor_id
}

fn emit_link_diagnostic(
    runtime: &Runtime,
    peer_node: NodeId,
    side: &str,
    sequence: i64,
    elapsed_ns: u128,
) {
    let (connection_count, connection_addr) = runtime
        .distributed
        .transport
        .as_ref()
        .map(|transport| (transport.connection_count(), transport.connection_addr(peer_node)))
        .unwrap_or((0, None));
    let cluster_addr = runtime
        .distributed
        .cluster
        .as_ref()
        .and_then(|cluster| cluster.get_node(peer_node))
        .map(|node| node.address);

    eprintln!(
        "[remote-rtt-diagnostic] side={side} sequence={sequence} process_network_ns={elapsed_ns} connections={connection_count} connection_addr={connection_addr:?} cluster_addr={cluster_addr:?}"
    );
}

fn drive_until(
    runtime: &mut Runtime,
    peer_node: NodeId,
    side: &str,
    actor_id: u64,
    sequence: i64,
) -> StageBudget {
    let deadline = Instant::now() + HOP_TIMEOUT;
    let mut budget = StageBudget::default();

    loop {
        budget.polls += 1;

        let process_started = Instant::now();
        runtime.process_network();
        let process_elapsed = process_started.elapsed();
        budget.process_network_ns += process_elapsed.as_nanos();
        if process_elapsed > Duration::from_millis(10) {
            emit_link_diagnostic(runtime, peer_node, side, sequence, process_elapsed.as_nanos());
        }

        let scheduler_started = Instant::now();
        runtime.run_scheduler();
        budget.run_scheduler_ns += scheduler_started.elapsed().as_nanos();

        let seen = runtime
            .actors
            .get(&actor_id)
            .and_then(|actor| actor.get_state_field("seen"))
            .and_then(|value| value.as_int());
        if seen == Some(sequence) {
            return budget;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for remote actor sequence {sequence}; last={seen:?}"
        );
        budget.yield_count += 1;
        thread::yield_now();
    }
}

struct LoopbackRoundTrip {
    left: Runtime,
    right: Runtime,
    left_node: NodeId,
    right_node: NodeId,
    left_actor: u64,
    right_actor: u64,
}

impl LoopbackRoundTrip {
    fn new() -> Self {
        let mut left = distributed_runtime();
        let mut right = distributed_runtime();
        let (left_node, left_addr) = endpoint(&left);
        let (right_node, right_addr) = endpoint(&right);

        make_peer_healthy(&mut left, right_node, right_addr);
        make_peer_healthy(&mut right, left_node, left_addr);

        // Connection establishment is explicitly outside both warm-up and the
        // measured region. Relying on the first asynchronous send to bootstrap
        // the link races sender-thread connection setup with cluster polling and
        // can turn a fixture problem into a 30-second IO timeout.
        establish_loopback_link(&mut left, &right, right_node, right_addr);

        let left_actor = spawn_probe(&mut left);
        let right_actor = spawn_probe(&mut right);
        Self {
            left,
            right,
            left_node,
            right_node,
            left_actor,
            right_actor,
        }
    }

    fn send(
        runtime: &mut Runtime,
        target_node: NodeId,
        target_actor: u64,
        sequence: i64,
    ) -> u128 {
        let started = Instant::now();
        runtime.send_distributed(
            ActorAddress::remote(target_node, target_actor),
            "record",
            &[Value::int(sequence)],
        );
        started.elapsed().as_nanos()
    }

    fn roundtrip(&mut self, sequence: i64) -> StageBudget {
        let mut budget = StageBudget::default();

        budget.send_ns += Self::send(
            &mut self.left,
            self.right_node,
            self.right_actor,
            sequence,
        );
        budget.add(drive_until(
            &mut self.right,
            self.left_node,
            "right",
            self.right_actor,
            sequence,
        ));

        budget.send_ns += Self::send(
            &mut self.right,
            self.left_node,
            self.left_actor,
            sequence,
        );
        budget.add(drive_until(
            &mut self.left,
            self.right_node,
            "left",
            self.left_actor,
            sequence,
        ));

        budget
    }
}

#[test]
#[ignore = "release-mode performance probe; run from remote RTT stage workflow"]
fn bench_ab_remote_rtt_stage_budget() {
    let mut fixture = LoopbackRoundTrip::new();
    for sequence in 0..WARMUP_ROUNDTRIPS {
        fixture.roundtrip(sequence as i64 + 1);
    }

    let mut budget = StageBudget::default();
    let started = Instant::now();
    for offset in 0..MEASURED_ROUNDTRIPS {
        budget.add(fixture.roundtrip((WARMUP_ROUNDTRIPS + offset + 1) as i64));
    }
    let elapsed_ns = started.elapsed().as_nanos();

    println!(
        "[remote-rtt-stage] roundtrips={MEASURED_ROUNDTRIPS} elapsed_ns={elapsed_ns} send_ns={} process_network_ns={} run_scheduler_ns={} polls={} yield_count={} ns_per_roundtrip={:.1} polls_per_roundtrip={:.2}",
        budget.send_ns,
        budget.process_network_ns,
        budget.run_scheduler_ns,
        budget.polls,
        budget.yield_count,
        elapsed_ns as f64 / MEASURED_ROUNDTRIPS as f64,
        budget.polls as f64 / MEASURED_ROUNDTRIPS as f64,
    );
}
// REMOTE_RTT_STAGE_PROFILE_END
