const BENCHMARK: &str = include_str!("distributed_stage_bench.rs");

fn distributed_stage_block() -> &'static str {
    let start_marker = "// DISTRIBUTED_STAGE_DECOMP_START";
    let end_marker = "// DISTRIBUTED_STAGE_DECOMP_END";
    let start = BENCHMARK
        .find(start_marker)
        .expect("benchmark must define a distributed-stage decomposition block");
    let end = BENCHMARK[start..]
        .find(end_marker)
        .map(|offset| start + offset)
        .expect("distributed-stage block must have an end marker");
    &BENCHMARK[start..end]
}

#[test]
fn distributed_stage_probe_separates_send_ingress_and_scheduler() {
    assert!(
        BENCHMARK.contains("DeterministicNetworkTransport"),
        "distributed-stage benchmark must use deterministic zero-latency transport"
    );

    let block = distributed_stage_block();

    for required in [
        "bench_ab_distributed_stage_decomposition",
        "send_distributed",
        "process_network();",
        "run_scheduler();",
        "distributed_send_enqueue",
        "distributed_ingress_mailbox",
        "distributed_scheduler_handler",
    ] {
        assert!(
            block.contains(required),
            "distributed-stage benchmark must contain {required:?}"
        );
    }

    for forbidden in ["TcpTransport", "thread::sleep", "Instant::now() +"] {
        assert!(
            !BENCHMARK.contains(forbidden),
            "distributed-stage benchmark must not include transport/wall-clock fixture {forbidden:?}"
        );
    }
}
