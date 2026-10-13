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
//! to preserve causal lineage across process and node boundaries.
//!
//! Without OpenTelemetry, each handled message records a `tracing` span
//! carrying logical `trace_id`, `span_id`, and `parent_span_id` fields,
//! with no span overhead when the subscriber filters TRACE.
//! With the optional `otel` feature, the W3C parent is installed as a real
//! remote OpenTelemetry context and outgoing messages use the actual SDK
//! span ID, not only text attributes.

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

    /// Extract at most one W3C traceparent header from an inbound HTTP request.
    /// Header names are case-insensitive. Duplicates fail closed rather than
    /// selecting whichever happens to appear first.
    pub fn from_http_headers(headers: &[(String, String)]) -> Option<Self> {
        let mut incoming: Option<&str> = None;
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("traceparent") {
                if incoming.is_some() {
                    return None;
                }
                incoming = Some(value.trim());
            }
        }
        Self::from_traceparent(incoming?)
    }

    /// Attach this exact context as a remote parent to a tracing span.
    /// This is the bridge from W3C transport metadata to *real* OTel
    /// SpanContext: text fields alone do not establish an exported parent.
    #[cfg(feature = "otel")]
    pub(crate) fn attach_as_otel_parent(&self, span: &tracing::Span) {
        use opentelemetry::trace::{
            SpanContext, SpanId, TraceContextExt as _, TraceFlags, TraceId, TraceState,
        };
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        let remote_parent = SpanContext::new(
            TraceId::from_bytes(self.trace_id.to_be_bytes()),
            SpanId::from_bytes(self.span_id.to_be_bytes()),
            TraceFlags::new(if self.sampled { 1 } else { 0 }),
            true,
            TraceState::default(),
        );
        let cx = opentelemetry::Context::new().with_remote_span_context(remote_parent);
        // If no OTel subscriber exists, tracing fields still work. Never
        // prevent actor or HTTP dispatch solely because tracing is disabled.
        let _ = span.set_parent(cx);
    }

    /// Read the actual exported span identity. tracing-opentelemetry assigns
    /// a fresh span ID independently of our actor trace token; outgoing
    /// messages must carry the *exported* ID to preserve real parentage.
    #[cfg(feature = "otel")]
    fn exported_span_identity(&self, span: &tracing::Span) -> Option<Self> {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        let cx = span.context();
        let current = cx.span();
        let id = current.span_context();
        if !id.is_valid() {
            return None;
        }
        Some(Self {
            trace_id: u128::from_be_bytes(id.trace_id().to_bytes()),
            span_id: u64::from_be_bytes(id.span_id().to_bytes()),
            parent_span_id: self.parent_span_id,
            sampled: id.is_sampled(),
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
        let span = tracing::span!(
            parent: None,
            tracing::Level::TRACE,
            "actor_msg",
            actor_id,
            behavior_id = behavior_idx,
            trace_id = %trace,
            span_id = %span,
            parent_span_id = %parent,
        );
        #[cfg(feature = "otel")]
        if self.parent_span_id != 0 {
            let parent = Self {
                trace_id: self.trace_id,
                span_id: self.parent_span_id,
                parent_span_id: 0,
                sampled: self.sampled,
            };
            parent.attach_as_otel_parent(&span);
        }
        span
    }

    /// Enter a dispatch span for this context, if any subscriber is attached
    /// at TRACE level. Returns `None` (no-op) otherwise, so the hot message
    /// path does no span work in a default build.
    pub fn enter_dispatch_span(
        &self,
        actor_id: u64,
        behavior_idx: usize,
    ) -> Option<tracing::span::EnteredSpan> {
        self.enter_dispatch_span_with_context(actor_id, behavior_idx)
            .0
    }

    /// Enter the actor span and return its actual exported identity for
    /// subsequent mailbox / network traceparent propagation. With no OTel
    /// subscriber or feature, retain the runtime's existing logical ID.
    pub(crate) fn enter_dispatch_span_with_context(
        &self,
        actor_id: u64,
        behavior_idx: usize,
    ) -> (Option<tracing::span::EnteredSpan>, Self) {
        if !tracing::enabled!(tracing::Level::TRACE) {
            return (None, *self);
        }
        let span = self.tracing_span(actor_id, behavior_idx);
        #[cfg(feature = "otel")]
        let effective = self.exported_span_identity(&span).unwrap_or(*self);
        #[cfg(not(feature = "otel"))]
        let effective = *self;
        #[cfg(feature = "otel")]
        {
            span.record(
                "trace_id",
                tracing::field::display(format!("{:032x}", effective.trace_id)),
            );
            span.record(
                "span_id",
                tracing::field::display(format!("{:016x}", effective.span_id)),
            );
        }
        (Some(span.entered()), effective)
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
    fn incoming_http_traceparent_is_case_insensitive_and_rejects_duplicates() {
        let headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            (
                "TrAcEpArEnT".to_owned(),
                " 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01 ".to_owned(),
            ),
        ];
        let parent = TraceContext::from_http_headers(&headers).expect("valid HTTP parent");
        assert_eq!(parent.trace_id(), 0x4bf92f3577b34da6a3ce929d0e0e4736);
        assert_eq!(parent.span_id(), 0x00f067aa0ba902b7);
        assert!(TraceContext::from_http_headers(&headers[..1]).is_none());
        let mut duplicate = headers.clone();
        duplicate.push(("traceparent".to_string(), headers[1].1.clone()));
        assert!(TraceContext::from_http_headers(&duplicate).is_none());
        let invalid = vec![("traceparent".to_string(), "malformed".to_string())];
        assert!(TraceContext::from_http_headers(&invalid).is_none());
    }

    #[cfg(feature = "otel")]
    #[test]
    fn exported_actor_span_reuses_actual_otel_identity_for_outgoing_messages() {
        use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        use tracing_subscriber::prelude::*;

        let provider = opentelemetry_sdk::trace::TracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("nulang-test")));
        tracing::subscriber::with_default(subscriber, || {
            let parent = TraceContext::from_traceparent(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )
            .unwrap();
            let child = parent.child();
            let (guard, emitted) = child.enter_dispatch_span_with_context(17, 2);
            let _guard = guard.expect("TRACE subscriber enables actor span");
            let active = tracing::Span::current().context();
            let actual_span = active.span();
            let context = actual_span.span_context();
            assert!(context.is_valid());
            assert_eq!(
                context.trace_id().to_bytes(),
                parent.trace_id().to_be_bytes()
            );
            assert_eq!(
                context.span_id().to_bytes(),
                emitted.span_id().to_be_bytes()
            );
            assert_eq!(emitted.parent_span_id(), parent.span_id());
            assert_ne!(emitted.span_id(), parent.span_id());
            assert_eq!(
                emitted.to_traceparent(),
                format!(
                    "00-{:032x}-{:016x}-01",
                    parent.trace_id(),
                    emitted.span_id()
                )
            );
        });
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
