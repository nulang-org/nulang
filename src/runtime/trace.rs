//! W3C trace context propagation for the actor runtime.
//!
//! The runtime threads a W3C `traceparent` string through every message so
//! causal chains span actor, shard, and node boundaries:
//!
//! * the local mailbox — [`Message::trace_id`](crate::runtime::mailbox::Message),
//! * the cross-shard channel — [`CrossShardMsg::DeliverMessage`],
//! * the NUL0 wire — [`Packet::ActorMessage`](crate::runtime::network::Packet),
//! * and the durable event journal (via [`Message::trace_id`]).
//!
//! [`TraceContext`] is a small (Copy) value: a 128-bit trace id plus the
//! current span id, its parent span id, and the W3C sampled flag. It
//! serializes to a standard `traceparent` string (`00-<32 hex>-<16 hex>-<flags>`)
//! so any W3C-compliant collector can reconstruct the hierarchy.
//!
//! The default (non-OTel) build records each handled message as a
//! `tracing` span carrying `trace_id` / `span_id` / `parent_span_id` as
//! structured fields — zero work when no `tracing` subscriber is attached.
//! The optional `otel` feature can bridge those fields into real OTLP spans
//! via `tracing-opentelemetry`.

use std::sync::atomic::{AtomicU64, Ordering};

/// Thread-safe stateless PRNG for trace / span id generation.
///
/// A monotonically incremented counter fed through splitmix64. Deterministic
/// seeding is fine here — trace ids are correlation keys, not a security
/// boundary, and this avoids pulling in `rand`/`getrandom`.
struct Rng {
    state: AtomicU64,
}

impl Rng {
    fn next_u64(&self) -> u64 {
        let mut z = self
            .state
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

static RNG: Rng = Rng {
    state: AtomicU64::new(0x4D59_5DF4_D0F3_3173),
};

fn nonzero_u64() -> u64 {
    loop {
        let v = RNG.next_u64();
        if v != 0 {
            return v;
        }
    }
}

fn nonzero_u128() -> u128 {
    let hi = nonzero_u64();
    let lo = RNG.next_u64();
    let v = ((hi as u128) << 64) | lo as u128;
    if v == 0 {
        1
    } else {
        v
    }
}

/// A W3C trace context: a 128-bit trace id plus the current span id, its
/// parent span id, and the sampled flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceContext {
    trace_id: u128,
    span_id: u64,
    parent_span_id: u64,
    sampled: bool,
}

impl TraceContext {
    /// Start a fresh trace at a root span.
    pub fn root() -> Self {
        TraceContext {
            trace_id: nonzero_u128(),
            span_id: nonzero_u64(),
            parent_span_id: 0,
            sampled: true,
        }
    }

    /// Create a child span under `self`: same trace, new span id, the parent
    /// set to `self.span_id`.
    pub fn child(&self) -> Self {
        TraceContext {
            trace_id: self.trace_id,
            span_id: nonzero_u64(),
            parent_span_id: self.span_id,
            sampled: self.sampled,
        }
    }

    /// Parse a W3C `traceparent` string into a context whose `span_id` is the
    /// *incoming* (sender's) span. Call [`child`](TraceContext::child) to
    /// obtain this side's own span under it. Returns `None` on malformed
    /// input, non-zero-length version, or a zero trace/span id.
    pub fn from_traceparent(s: &str) -> Option<Self> {
        // Version 00 has a fixed 55-byte wire layout. Validate its byte
        // offsets before slicing; str::get rejects multibyte UTF-8 boundaries
        // instead of panicking on malformed incoming network data.
        let bytes = s.as_bytes();
        if bytes.len() != 55 || &bytes[..3] != b"00-" || bytes[35] != b'-' || bytes[52] != b'-' {
            return None;
        }
        let trace = s.get(3..35)?;
        let span = s.get(36..52)?;
        let flags = s.get(53..55)?;
        // Numeric radix parsers permit a leading '+', but the W3C wire
        // format allows only hexadecimal digits in each fixed-width field.
        if trace.starts_with('+') || span.starts_with('+') || flags.starts_with('+') {
            return None;
        }
        let trace_id = u128::from_str_radix(trace, 16).ok()?;
        let span_id = u64::from_str_radix(span, 16).ok()?;
        let trace_flags = u8::from_str_radix(flags, 16).ok()?;
        if trace_id == 0 || span_id == 0 {
            return None;
        }
        Some(TraceContext {
            trace_id,
            span_id,
            parent_span_id: 0,
            sampled: trace_flags & 0x01 != 0,
        })
    }

    /// Encode as a W3C `traceparent` string.
    pub fn to_traceparent(&self) -> String {
        let flags = if self.sampled { "01" } else { "00" };
        format!("00-{:032x}-{:016x}-{flags}", self.trace_id, self.span_id)
    }

    pub fn trace_id(&self) -> u128 {
        self.trace_id
    }

    pub fn span_id(&self) -> u64 {
        self.span_id
    }

    pub fn parent_span_id(&self) -> u64 {
        self.parent_span_id
    }

    pub fn sampled(&self) -> bool {
        self.sampled
    }

    /// Build a `tracing` span for a handled message, carrying this context
    /// as structured fields. `parent: None` forces the span to be a fresh
    /// root in the `tracing` tree — the W3C parentage is expressed through
    /// the explicit `parent_span_id` field, which an OTel bridge can use to
    /// rebuild the true hierarchy.
    ///
    /// The hex strings are allocated eagerly, so callers SHOULD gate this on
    /// [`tracing::enabled!`](tracing::enabled) (see
    /// [`enter_dispatch_span`](TraceContext::enter_dispatch_span)).
    pub fn tracing_span(&self, actor_id: u64, behavior_idx: usize) -> tracing::Span {
        let trace = format!("{:032x}", self.trace_id);
        let span = format!("{:016x}", self.span_id);
        let parent = format!("{:016x}", self.parent_span_id);
        tracing::span!(
            parent: None,
            tracing::Level::TRACE,
            "actor_msg",
            actor_id,
            behavior_id = behavior_idx,
            trace_id = %trace,
            span_id = %span,
            parent_span_id = %parent,
        )
    }

    /// Enter a dispatch span for this context, if any subscriber is attached
    /// at TRACE level. Returns `None` (no-op) otherwise, so the hot message
    /// path does no span work in a default build.
    pub fn enter_dispatch_span(
        &self,
        actor_id: u64,
        behavior_idx: usize,
    ) -> Option<tracing::span::EnteredSpan> {
        if !tracing::enabled!(tracing::Level::TRACE) {
            return None;
        }
        Some(self.tracing_span(actor_id, behavior_idx).entered())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_traceparent_roundtrip() {
        let ctx = TraceContext::root();
        let tp = ctx.to_traceparent();
        let parsed = TraceContext::from_traceparent(&tp).expect("parse own output");
        assert_eq!(parsed.trace_id(), ctx.trace_id());
        assert_eq!(parsed.span_id(), ctx.span_id());
        assert_eq!(parsed.sampled(), ctx.sampled());
    }

    #[test]
    fn test_traceparent_format() {
        // 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01 (W3C spec example)
        let ctx = TraceContext::from_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        )
        .expect("spec example parses");
        assert_eq!(ctx.trace_id(), 0x4bf9_2f35_77b3_4da6_a3ce_929d_0e0e_4736);
        assert_eq!(ctx.span_id(), 0x00f0_67aa_0ba9_02b7);
        assert!(ctx.sampled());
    }

    #[test]
    fn test_child_keeps_trace_links_parent() {
        let root = TraceContext::root();
        let child = root.child();
        assert_eq!(child.trace_id(), root.trace_id());
        assert_eq!(child.parent_span_id(), root.span_id());
        assert_ne!(child.span_id(), root.span_id());
        assert_eq!(child.sampled(), root.sampled());
    }

    #[test]
    fn test_malformed_traceparent_rejected() {
        assert!(TraceContext::from_traceparent("").is_none());
        assert!(TraceContext::from_traceparent("00-abc-123-01").is_none()); // wrong lengths
        assert!(TraceContext::from_traceparent("00-zz..-..-..").is_none()); // bad hex
        assert!(
            TraceContext::from_traceparent(
                "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            )
            .is_none() // version != 00
        );
        assert!(
            TraceContext::from_traceparent(
                "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
            )
            .is_none() // zero trace id
        );
    }

    #[test]
    fn test_traceparent_fixed_width_parser_preserves_w3c_validation() {
        const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let reference = TraceContext::from_traceparent(VALID).expect("W3C spec context");
        let uppercase = VALID.to_ascii_uppercase();
        let upper = TraceContext::from_traceparent(&uppercase).expect("hex may be uppercase");
        assert_eq!(upper, reference);

        // Every character is significant, including the fixed separators.
        for i in 0..VALID.len() {
            let mut altered = VALID.as_bytes().to_vec();
            altered[i] = b'Z';
            let altered = std::str::from_utf8(&altered).expect("ASCII candidate");
            assert!(
                TraceContext::from_traceparent(altered).is_none(),
                "invalid byte at offset {i} must be rejected"
            );
        }

        // UTF-8 with the same number of bytes must never panic on fixed offsets.
        let mut nonascii = VALID.to_string();
        nonascii.replace_range(3..5, "é");
        assert!(TraceContext::from_traceparent(&nonascii).is_none());

        // Rust numeric parsing accepts an optional plus sign; W3C hex does not.
        let mut signed = VALID.to_string();
        signed.replace_range(3..4, "+");
        assert!(TraceContext::from_traceparent(&signed).is_none());

        // The version, trace ID, parent span ID, and flags must still validate.
        assert!(TraceContext::from_traceparent(
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
        )
        .is_none());
        assert!(TraceContext::from_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01"
        )
        .is_none());
        assert!(TraceContext::from_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-zz"
        )
        .is_none());
        assert!(!TraceContext::from_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"
        )
        .expect("unsampled trace")
        .sampled());
    }

    #[test]
    fn test_root_ids_are_nonzero_and_distinct() {
        let a = TraceContext::root();
        let b = TraceContext::root();
        assert_ne!(a.trace_id(), 0);
        assert_ne!(a.span_id(), 0);
        assert_ne!(a.trace_id(), b.trace_id());
    }
}
