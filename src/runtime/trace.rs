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

use std::fmt;
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

/// Allocation-free W3C field formatters. Tracing visitors consume these
/// synchronously when an enabled span is constructed, so there is no need to
/// allocate three intermediate hex strings per actor dispatch.
struct TraceHexId(u128);

impl fmt::Display for TraceHexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

struct SpanHexId(u64);

impl fmt::Display for SpanHexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
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
        let mut parts = s.split('-');
        let version = parts.next()?;
        let trace = parts.next()?;
        let span = parts.next()?;
        let flags = parts.next()?;
        if parts.next().is_some() {
            return None; // too many fields
        }
        if version.len() != 2 || trace.len() != 32 || span.len() != 16 || flags.len() != 2 {
            return None;
        }
        let mut version_buf = [0u8; 1];
        hex::decode_to_slice(version, &mut version_buf).ok()?;
        if version_buf[0] != 0 {
            return None; // only version 00 is understood
        }
        let mut trace_buf = [0u8; 16];
        hex::decode_to_slice(trace, &mut trace_buf).ok()?;
        let mut span_buf = [0u8; 8];
        hex::decode_to_slice(span, &mut span_buf).ok()?;
        let mut flags_buf = [0u8; 1];
        hex::decode_to_slice(flags, &mut flags_buf).ok()?;
        let trace_id = u128::from_be_bytes(trace_buf);
        let span_id = u64::from_be_bytes(span_buf);
        if trace_id == 0 || span_id == 0 {
            return None;
        }
        Some(TraceContext {
            trace_id,
            span_id,
            parent_span_id: 0,
            sampled: flags_buf[0] & 0x01 != 0,
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
    /// The W3C fields are formatted directly into the tracing visitor
    /// without allocating temporary String values. The dispatch helper also
    /// guards against constructing disabled spans.
    pub fn tracing_span(&self, actor_id: u64, behavior_idx: usize) -> tracing::Span {
        let trace = TraceHexId(self.trace_id);
        let span = SpanHexId(self.span_id);
        let parent = SpanHexId(self.parent_span_id);
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
    fn formatted_trace_ids_keep_w3c_width_without_heap_strings() {
        assert_eq!(
            format!("{}", TraceHexId(0xabu128)),
            format!("{:032x}", 0xabu128)
        );
        assert_eq!(
            format!("{}", SpanHexId(0xabu64)),
            format!("{:016x}", 0xabu64)
        );
        assert_eq!(format!("{}", SpanHexId(0)), "0000000000000000");
    }

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
    fn test_root_ids_are_nonzero_and_distinct() {
        let a = TraceContext::root();
        let b = TraceContext::root();
        assert_ne!(a.trace_id(), 0);
        assert_ne!(a.span_id(), 0);
        assert_ne!(a.trace_id(), b.trace_id());
    }
}
