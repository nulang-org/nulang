//! Standalone steady-state remote actor round-trip benchmark.
//!
//! The runner is intentionally small and uses the same production TCP/NUL0
//! path as the runtime. Startup, cluster seeding, TCP establishment, and warm-up
//! round trips happen before timing. One measured operation is a one-value
//! remote actor message left→right followed by the same return hop right→left.

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use nulang::runtime::{Actor, ActorAddress, NodeId, Runtime, TlsConfig};
use nulang::vm::Value;
use serde_json::json;

const HOP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
enum OutputFormat {
    Human,
    Jsonl,
}

struct Config {
    roundtrips: u64,
    warmup: u64,
    repeat: u32,
    format: OutputFormat,
}

fn main() -> ExitCode {
    let config = match parse_args() {
        Ok(Some(config)) => config,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    for iteration in 1..=config.repeat {
        let mut fixture = LoopbackRoundTrip::new();
        for sequence in 0..config.warmup {
            fixture.roundtrip(sequence as i64 + 1);
        }

        let start_sequence = config.warmup + 1;
        let started = Instant::now();
        for offset in 0..config.roundtrips {
            let sequence =
                i64::try_from(start_sequence + offset).expect("benchmark sequence must fit in i64");
            fixture.roundtrip(sequence);
        }
        let elapsed = started.elapsed();
        emit(iteration, config.roundtrips, elapsed, config.format);
    }

    ExitCode::SUCCESS
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut roundtrips = 10_000u64;
    let mut warmup = 100u64;
    let mut repeat = 1u32;
    let mut format = OutputFormat::Human;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--roundtrips" => {
                roundtrips = parse_positive_u64(
                    args.next()
                        .ok_or_else(|| "--roundtrips requires N".to_string())?,
                    "roundtrips",
                )?;
            }
            "--warmup" => {
                warmup = args
                    .next()
                    .ok_or_else(|| "--warmup requires N".to_string())?
                    .parse::<u64>()
                    .map_err(|_| "--warmup must be a non-negative integer".to_string())?;
            }
            "--repeat" => {
                let raw = args
                    .next()
                    .ok_or_else(|| "--repeat requires N".to_string())?;
                repeat = raw
                    .parse::<u32>()
                    .map_err(|_| "--repeat must be a positive integer".to_string())?;
                if repeat == 0 {
                    return Err("--repeat must be at least 1".to_string());
                }
            }
            "--format" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--format requires human or jsonl".to_string())?;
                format = match value.as_str() {
                    "human" => OutputFormat::Human,
                    "jsonl" => OutputFormat::Jsonl,
                    _ => return Err(format!("unsupported format: {value}")),
                };
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    Ok(Some(Config {
        roundtrips,
        warmup,
        repeat,
        format,
    }))
}

fn parse_positive_u64(raw: String, label: &str) -> Result<u64, String> {
    let value = raw
        .parse::<u64>()
        .map_err(|_| format!("--{label} must be a positive integer"))?;
    if value == 0 {
        return Err(format!("--{label} must be at least 1"));
    }
    Ok(value)
}

fn print_usage() {
    eprintln!(
        "Usage: nulang-remote-roundtrip [--roundtrips N] [--warmup N] [--repeat N] [--format human|jsonl]"
    );
}

fn emit(iteration: u32, roundtrips: u64, elapsed: Duration, format: OutputFormat) {
    let elapsed_ns =
        u64::try_from(elapsed.as_nanos()).expect("benchmark duration must fit in u64 nanoseconds");
    let ns_per_roundtrip = elapsed_ns as f64 / roundtrips as f64;
    match format {
        OutputFormat::Human => println!(
            "[remote-roundtrip] runtime=nulang iteration={iteration} roundtrips={roundtrips} elapsed_ns={elapsed_ns} ns_per_roundtrip={ns_per_roundtrip:.1}"
        ),
        OutputFormat::Jsonl => println!(
            "{}",
            json!({
                "schema": 1,
                "runtime": "nulang",
                "suite": "remote-actor-roundtrip",
                "iteration": iteration,
                "roundtrips": roundtrips,
                "elapsed_ns": elapsed_ns,
                "ns_per_roundtrip": ns_per_roundtrip,
            })
        ),
    }
}

fn record_sequence(actor: &mut Actor, args: &[Value]) {
    if let Some(sequence) = args.first().and_then(|value| value.as_int()) {
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

fn spawn_probe(runtime: &mut Runtime) -> u64 {
    let actor_id = runtime.spawn_actor(Box::new(|| vec![("seen".to_string(), Value::int(0))]));
    runtime
        .actors
        .get_mut(&actor_id)
        .expect("spawned actor should exist")
        .register_behavior("record", record_sequence);
    runtime.run_scheduler();
    actor_id
}

fn drive_until(runtime: &mut Runtime, actor_id: u64, sequence: i64) {
    let deadline = Instant::now() + HOP_TIMEOUT;
    loop {
        runtime.process_network();
        runtime.run_scheduler();
        let seen = runtime
            .actors
            .get(&actor_id)
            .and_then(|actor| actor.get_state_field("seen"))
            .and_then(|value| value.as_int());
        if seen == Some(sequence) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for remote actor sequence {sequence}; last={seen:?}"
        );
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

        let left_actor = spawn_probe(&mut left);
        let right_actor = spawn_probe(&mut right);
        let mut fixture = Self {
            left,
            right,
            left_node,
            right_node,
            left_actor,
            right_actor,
        };

        // Establish the actual TCP/NUL0 path before caller-controlled warm-up.
        fixture.roundtrip(-1);
        fixture
    }

    fn roundtrip(&mut self, sequence: i64) {
        self.left.send_distributed(
            ActorAddress::remote(self.right_node, self.right_actor),
            "record",
            &[Value::int(sequence)],
        );
        drive_until(&mut self.right, self.right_actor, sequence);

        self.right.send_distributed(
            ActorAddress::remote(self.left_node, self.left_actor),
            "record",
            &[Value::int(sequence)],
        );
        drive_until(&mut self.left, self.left_actor, sequence);
    }
}
