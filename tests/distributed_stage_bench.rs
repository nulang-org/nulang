use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nulang::runtime::{Actor, ActorAddress, DeterministicNetworkTransport, NodeId, Runtime};
use nulang::vm::Value;

fn noop(_actor: &mut Actor, _args: &[Value]) {}

fn distributed_runtime(
    addr: SocketAddr,
    bus: Arc<
        parking_lot::Mutex<
            HashMap<
                NodeId,
                (
                    std::sync::mpsc::SyncSender<nulang::runtime::IncomingPacket>,
                    std::sync::mpsc::SyncSender<nulang::runtime::OutgoingPacket>,
                ),
            >,
        >,
    >,
) -> Runtime {
    let mut runtime = Runtime::new();
    runtime.install_virtual_clock();
    let transport =
        DeterministicNetworkTransport::bind_with_bus(addr, bus).expect("transport should bind");
    transport.register_on_bus();
    runtime
        .enable_distribution_with_transport(Box::new(transport))
        .expect("distribution should enable");
    runtime
}

fn report_stage(name: &str, operations: u64, elapsed: Duration) {
    let ns_per_op = elapsed.as_nanos() as f64 / operations as f64;
    println!(
        "[ab-bench] benchmark={name} operations={operations} elapsed_ns={} ns_per_op={ns_per_op:.1}",
        elapsed.as_nanos()
    );
}

// DISTRIBUTED_STAGE_DECOMP_START
/// Decompose the single-message runtime work around distributed actor delivery
/// while removing real network latency from the experiment.
///
/// `distributed_send_enqueue` measures sender-side actor-address resolution,
/// packet/message preparation, and enqueue into the zero-latency deterministic
/// transport. `distributed_ingress_mailbox` then measures one receiver
/// `process_network()` turn: packet drain, ActorMessage routing, behavior-name
/// resolution, message construction, mailbox admission, ready publication,
/// and ACK emission. `distributed_scheduler_handler` finally measures one
/// `run_scheduler()` turn that dispatches the admitted message to a no-op
/// native handler.
#[test]
#[ignore = "release-mode performance probe; run from distributed-stage workflow"]
fn bench_ab_distributed_stage_decomposition() {
    const ITERATIONS: u64 = 10_000;

    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let sender_addr: SocketAddr = "127.0.0.1:39201".parse().unwrap();
    let receiver_addr: SocketAddr = "127.0.0.1:39202".parse().unwrap();
    let sender_node = NodeId::new(&sender_addr);
    let receiver_node = NodeId::new(&receiver_addr);

    let mut sender = distributed_runtime(sender_addr, bus.clone());
    let mut receiver = distributed_runtime(receiver_addr, bus);

    // Pin a healthy peer relationship without advancing virtual time. This
    // keeps periodic cluster maintenance out of all timed regions.
    sender
        .distributed
        .cluster
        .as_mut()
        .expect("sender cluster")
        .handle_heartbeat(receiver_node, receiver_addr);
    receiver
        .distributed
        .cluster
        .as_mut()
        .expect("receiver cluster")
        .handle_heartbeat(sender_node, sender_addr);

    let target = receiver.spawn_actor(Box::new(Vec::new));
    receiver
        .actors
        .get_mut(&target)
        .expect("spawned target")
        .register_behavior("handle", noop);
    receiver.run_scheduler();

    let mut send_elapsed = Duration::ZERO;
    let mut ingress_elapsed = Duration::ZERO;
    let mut scheduler_elapsed = Duration::ZERO;

    for sequence in 0..ITERATIONS {
        // Timed boundary 1: one sender runtime turn plus enqueue into the
        // zero-latency deterministic transport. There is no TCP/socket wait.
        let send_start = Instant::now();
        sender.send_distributed(
            ActorAddress::remote(receiver_node, target),
            "handle",
            &[Value::int(sequence as i64)],
        );
        send_elapsed += send_start.elapsed();

        // Timed boundary 2: exactly one packet is already present in the
        // deterministic transport. Fixed per-call runtime work is therefore
        // retained instead of amortized across a throughput batch.
        let ingress_start = Instant::now();
        receiver.process_network();
        ingress_elapsed += ingress_start.elapsed();

        assert_eq!(
            receiver
                .actors
                .get(&target)
                .expect("target remains live")
                .mailbox
                .len(),
            1,
            "process_network must admit exactly one benchmark message"
        );

        // Timed boundary 3: network ingress is complete; measure one scheduler
        // turn and one no-op handler dispatch, matching the RTT runner's shape.
        let scheduler_start = Instant::now();
        receiver.run_scheduler();
        scheduler_elapsed += scheduler_start.elapsed();

        assert!(
            receiver
                .actors
                .get(&target)
                .expect("target remains live")
                .mailbox
                .is_empty(),
            "scheduler must drain the admitted benchmark message"
        );

        // ACK processing belongs to none of the receiver stages and is drained
        // outside all timed regions so sender-side bookkeeping stays bounded.
        sender.process_network();
    }

    report_stage("distributed_send_enqueue", ITERATIONS, send_elapsed);
    report_stage("distributed_ingress_mailbox", ITERATIONS, ingress_elapsed);
    report_stage("distributed_scheduler_handler", ITERATIONS, scheduler_elapsed);
}
// DISTRIBUTED_STAGE_DECOMP_END
