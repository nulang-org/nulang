# OpenTelemetry trace interoperability

**Status:** Experimental. This implementation covers HTTP ingress spans and
actor-message spans with explicit W3C traceparent. It does not make all
external effects automatically instrumented or establish a universal HTTP →
actor → database → HTTP client trace.

## Behavior

1. The native `Http.serve` listener and Nulang Web dev server parse
   `traceparent` from incoming HTTP/1.1 headers using case-insensitive header
   names. Exactly one valid W3C version-00 value is accepted; duplicates
   and malformed values are ignored, starting a new trace.
2. With the `otel` Cargo feature and TRACE-level `tracing` enabled, each
   HTTP request creates a server processing span. The decoded HTTP sender
   context becomes a **real remote OpenTelemetry parent**, not merely text
   attributes. Only the HTTP method is used as a span field; raw URLs, headers
   and bodies are not recorded.
3. For actor sends/receives, the existing NUL0, cross-shard and mailbox
   traceparent transport is unchanged. Each receiver's actor span uses
   the sender's remote context as its OpenTelemetry parent.
4. The outbound actor traceparent now uses the **actual OTel-generated span
   ID** when a recording subscriber is present, keeping network parent
   references aligned with the exported span. Without `otel` or an active
   TRACE subscriber, legacy logical trace IDs are preserved.

## Enable

Build with the optional OpenTelemetry feature:

```sh
cargo build --locked --features otel --bin nulang
```

The embedding process configures `nulang::observability::init_tracer()`
with an OTLP HTTP trace endpoint and calls `init_tracing()` before serving.
Set `RUST_LOG=trace` (or a suitable module-specific filter), since TRACE
actor and HTTP spans are deliberately not collected by the default `warn`
subscriber filter. Existing setup may also initialize metrics with
`init_meter()`. For example, collectors commonly expose an OTLP/HTTP
traces endpoint ending in `/v1/traces`.

## Conformance

```sh
cargo test --locked --no-default-features --lib incoming_http_traceparent_is_case_insensitive_and_rejects_duplicates
cargo test --locked --no-default-features --features otel --lib exported_actor_span_reuses_actual_otel_identity_for_outgoing_messages
cargo test --locked --no-default-features --features otel --lib http_ingress_uses_remote_traceparent_as_real_otel_parent
cargo check --locked --no-default-features --features otel --all-targets
```

These tests exercise context conversion, remote-parent trace inheritance,
and exported actor-span identity in-process. **They are not an OTLP collector
round trip or an HTTP-to-actor end-to-end integration test.**

## Boundaries and remaining gaps

- An HTTP request span is present only when the configured subscriber enables
  TRACE. `tracing` attribute names are intentionally bounded to avoid
  capturing tokens or personally identifiable information.
- The native HTTP serving path does not yet automatically seed an actor
  runtime context for work launched asynchronously from an HTTP handler.
- Arbitrary outbound `Http.get/post`, database calls, Python/native calls,
  LLM/tool calls, and durable workflow retries still require explicit
  client spans and context propagation tests.
- `tracestate`, `baggage`, sampling policy, and propagation across background
  task handoffs are not yet first-class runtime contracts.
- A complete production gate needs a local OpenTelemetry collector or an
  in-memory span exporter test asserting `parent_span_id` in **finished**
  exported spans across an HTTP → actor → actor fixture.
- Do not put secrets in trace fields or trust incoming trace IDs as an
  authentication or authorization mechanism.
