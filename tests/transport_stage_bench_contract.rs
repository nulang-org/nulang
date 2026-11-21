const DIST_BENCH: &str = include_str!("../benches/dist_bench.rs");

fn transport_stage_block() -> &'static str {
    let start_marker = "// TRANSPORT_STAGE_BENCH_START";
    let end_marker = "// TRANSPORT_STAGE_BENCH_END";
    let start = DIST_BENCH
        .find(start_marker)
        .expect("distribution benchmark must define a transport-stage block");
    let end = DIST_BENCH[start..]
        .find(end_marker)
        .map(|offset| start + offset)
        .expect("transport-stage block must have an end marker");
    &DIST_BENCH[start..end]
}

#[test]
fn transport_stage_probe_uses_transport_without_actor_runtime() {
    let block = transport_stage_block();

    for required in [
        "bench_transport_roundtrip",
        "dist/transport_roundtrip",
        "TcpTransport",
        "Packet::Heartbeat",
        "tcp_plaintext_heartbeat",
        "tcp_plaintext_actor_message_1",
        "actor_message_packet(1)",
        ".send(",
        ".receive()",
    ] {
        assert!(
            block.contains(required),
            "transport-stage benchmark must contain {required:?}"
        );
    }

    for forbidden in [
        "Runtime::",
        "ActorAddress",
        "send_distributed",
        "process_network",
        "run_scheduler",
        "mailbox",
    ] {
        assert!(
            !block.contains(forbidden),
            "transport-stage benchmark must exclude actor/runtime layer {forbidden:?}"
        );
    }
}
