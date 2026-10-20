//! Deterministic property-style conformance for the public trace context API.
//!
//! The seed and iteration count are fixed for reproducible failures. This is
//! intentionally dependency-free and does not change runtime or wire semantics.

use nulang::runtime::TraceContext;

const CASES: usize = 2_048;

fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[test]
fn traceparent_round_trip_preserves_identity_and_sampling_for_generated_inputs() {
    let mut seed = 0x5D54_EA43_80BE_5AFEu64;

    for i in 0..CASES {
        let trace_id = ((next_u64(&mut seed) as u128) << 64)
            | (next_u64(&mut seed) as u128);
        let span_id = next_u64(&mut seed);
        // Zero identifiers are invalid by contract; skip those inputs.
        if trace_id == 0 || span_id == 0 {
            continue;
        }
        let sampled = i % 2 == 0;
        let flags = if sampled { "01" } else { "00" };
        let wire = format!("00-{trace_id:032x}-{span_id:016x}-{flags}");

        let parsed = TraceContext::from_traceparent(&wire).expect("valid generated header");
        assert_eq!(parsed.trace_id(), trace_id, "case {i}");
        assert_eq!(parsed.span_id(), span_id, "case {i}");
        assert_eq!(parsed.parent_span_id(), 0, "case {i}");
        assert_eq!(parsed.sampled(), sampled, "case {i}");
        assert_eq!(parsed.to_traceparent(), wire, "case {i}");
    }
}

#[test]
fn trace_children_keep_causality_and_sampling_across_many_generations() {
    for _ in 0..CASES {
        let root = TraceContext::root();
        let first = root.child();
        let second = first.child();

        assert_ne!(root.trace_id(), 0);
        assert_ne!(first.span_id(), 0);
        assert_ne!(second.span_id(), 0);
        assert_eq!(first.trace_id(), root.trace_id());
        assert_eq!(second.trace_id(), root.trace_id());
        assert_eq!(first.parent_span_id(), root.span_id());
        assert_eq!(second.parent_span_id(), first.span_id());
        assert_eq!(second.sampled(), root.sampled());
        assert_ne!(root.span_id(), first.span_id());
        assert_ne!(first.span_id(), second.span_id());
    }

    let unsampled = TraceContext::from_traceparent(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
    )
    .expect("valid unsampled header");
    let child = unsampled.child();
    assert!(!child.sampled());
    assert_eq!(child.parent_span_id(), unsampled.span_id());
    assert!(child.to_traceparent().ends_with("-00"));
}

#[test]
fn invalid_hex_bytes_in_trace_and_span_ids_are_always_rejected() {
    let mut seed = 0xE4C2_91A7_55D8_0FB3u64;
    for case in 0..128 {
        let mut trace_id = ((next_u64(&mut seed) as u128) << 64)
            | next_u64(&mut seed) as u128;
        let mut span_id = next_u64(&mut seed);
        if trace_id == 0 {
            trace_id = 1;
        }
        if span_id == 0 {
            span_id = 1;
        }
        let wire = format!("00-{trace_id:032x}-{span_id:016x}-01");
        let bytes = wire.as_bytes();
        // The trace ID occupies bytes [3,35); span ID bytes [36,52).
        // Inject an invalid ASCII hexadecimal byte at every ID position.
        for index in (3..35).chain(36..52) {
            let mut corrupted = bytes.to_vec();
            corrupted[index] = b'g';
            let corrupted = String::from_utf8(corrupted).unwrap();
            assert!(
                TraceContext::from_traceparent(&corrupted).is_none(),
                "case {case}, byte {index}: {corrupted}"
            );
        }
    }
}

#[test]
fn malformed_wire_envelopes_fail_closed() {
    let good = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    assert!(TraceContext::from_traceparent(good).is_some());
    for bad in [
        "",
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
        "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-gg",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
    ] {
        assert!(
            TraceContext::from_traceparent(bad).is_none(),
            "unexpectedly accepted {bad}"
        );
    }
}
