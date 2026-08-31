//! OpenTelemetry observability wiring: trace and metric exporters over OTLP/HTTP.
//!
//! This module is compiled only when the `otel` feature is enabled.  It provides
//! `init_tracer` and `init_meter` helpers that configure an OTLP exporter
//! pointing at a collector URL (e.g. `http://localhost:4318`).  Both helpers
//! are idempotent — calling them twice with the same arguments returns the
//! existing tracer / meter provider without creating a duplicate pipeline.
//!
//! # Usage
//!
//! ```no_run
//! # #[cfg(feature = "otel")]
//! # {
//! use nulang::observability::{init_tracer, init_meter};
//! init_tracer("http://localhost:4318/v1/traces", "nulang-runtime").unwrap();
//! init_meter("http://localhost:4318/v1/metrics", "nulang-runtime").unwrap();
//! # }
//! ```

use parking_lot::Mutex;

use opentelemetry::global;
use opentelemetry_otlp::WithExportConfig;

/// Global singleton guard so `init_tracer` / `init_meter` are idempotent.
static TRACER_INIT: Mutex<bool> = Mutex::new(false);
static METER_INIT: Mutex<bool> = Mutex::new(false);

/// Initialise a global OTLP trace exporter.
///
/// `url` is the full collector endpoint, e.g.
/// `http://localhost:4318/v1/traces`.  `service_name` is used as the
/// OpenTelemetry `service.name` resource attribute.
///
/// Returns `Ok(())` on success, or `Err(String)` if the exporter pipeline
/// could not be built.  Second and subsequent calls return `Ok(())` without
/// creating additional pipelines.
pub fn init_tracer(url: &str, service_name: &str) -> Result<(), String> {
    let mut guard = TRACER_INIT.lock();
    if *guard {
        return Ok(());
    }

    let provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(opentelemetry_otlp::new_exporter().http().with_endpoint(url))
        .with_trace_config(opentelemetry_sdk::trace::Config::default().with_resource(
            opentelemetry_sdk::Resource::new(vec![
                opentelemetry::KeyValue::new("service.name", service_name.to_string()),
                opentelemetry::KeyValue::new(
                    "service.version",
                    crate::format::constants::LANGUAGE_VERSION_STR.to_string(),
                ),
            ]),
        ))
        .install_simple()
        .map_err(|e| format!("failed to build OTLP trace pipeline: {e}"))?;

    // Keep the concrete SDK provider so `init_tracing` can build an
    // `opentelemetry_sdk::trace::Tracer` (which implements
    // `tracing_opentelemetry::PreSampledTracer`).
    *TRACER_PROVIDER.lock() = Some(provider.clone());
    global::set_tracer_provider(provider);
    *guard = true;
    Ok(())
}

/// Initialise a global OTLP metric exporter.
///
/// `url` is the full collector endpoint, e.g.
/// `http://localhost:4318/v1/metrics`.  `service_name` is used as the
/// OpenTelemetry `service.name` resource attribute.
///
/// Returns `Ok(())` on success, or `Err(String)` if the exporter pipeline
/// could not be built.  Second and subsequent calls return `Ok(())` without
/// creating additional pipelines.
pub fn init_meter(url: &str, service_name: &str) -> Result<(), String> {
    let mut guard = METER_INIT.lock();
    if *guard {
        return Ok(());
    }

    let provider = opentelemetry_otlp::new_pipeline()
        .metrics(opentelemetry_sdk::runtime::TokioCurrentThread)
        .with_exporter(opentelemetry_otlp::new_exporter().http().with_endpoint(url))
        .with_resource(opentelemetry_sdk::Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", service_name.to_string()),
            opentelemetry::KeyValue::new(
                "service.version",
                crate::format::constants::LANGUAGE_VERSION_STR.to_string(),
            ),
        ]))
        .build()
        .map_err(|e| format!("failed to build OTLP metrics pipeline: {e}"))?;

    global::set_meter_provider(provider);
    *guard = true;
    Ok(())
}

/// Shut down the global tracer provider, flushing any buffered telemetry
/// before exit.  This is a best-effort operation; failures are ignored.
pub fn shutdown() {
    let mut tg = TRACER_INIT.lock();
    if *tg {
        let _ = global::shutdown_tracer_provider();
        *tg = false;
    }
    // Meter provider shutdown is a no-op in this build; the global provider
    // does not expose a typed shutdown method.
    let mut mg = METER_INIT.lock();
    *mg = false;
}

/// Whether the tracing subscriber has been installed by [`init_tracing`].
static TRACING_INIT: Mutex<bool> = Mutex::new(false);

/// Concrete SDK tracer provider created by [`init_tracer`], kept so
/// [`init_tracing`] can build an `opentelemetry_sdk::trace::Tracer` (the
/// `tracing_opentelemetry` layer requires a `PreSampledTracer`, which the
/// boxed global tracer is not).
static TRACER_PROVIDER: Mutex<Option<opentelemetry_sdk::trace::TracerProvider>> = Mutex::new(None);

/// Install a `tracing` subscriber that forwards spans to both the terminal
/// (formatted layer, filtered by `RUST_LOG`) and OpenTelemetry/OTLP, using
/// the global tracer provider configured by [`init_tracer`].
///
/// Idempotent: the first call installs the subscriber; later calls are no-ops.
/// Call after [`init_tracer`] so spans reach the OTLP exporter; if no tracer
/// provider is configured the layer forwards to a no-op tracer (spans are
/// dropped) while terminal logging still works.
pub fn init_tracing(service_name: &str) -> Result<(), String> {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let mut guard = TRACING_INIT.lock();
    if *guard {
        return Ok(());
    }
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // stderr, never stdout: `--lsp` must keep stdout pure JSON-RPC framing,
    // and CLI logs must not pollute piped program output.
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr);
    // Use the configured SDK provider when available; otherwise fall back to
    // a no-op SDK provider so terminal logging still works and spans are
    // dropped (the layer requires a `PreSampledTracer`).
    let tracer = TRACER_PROVIDER
        .lock()
        .as_ref()
        .map(|p| p.tracer(service_name.to_owned()))
        .unwrap_or_else(|| {
            opentelemetry_sdk::trace::TracerProvider::default().tracer(service_name.to_owned())
        });
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .init();
    *guard = true;
    Ok(())
}

// ---------------------------------------------------------------------------
// OTLP metric publishing (parity with Prometheus metrics)
// ---------------------------------------------------------------------------

use opentelemetry::metrics::Meter;

/// Cached OpenTelemetry metric instruments for publishing [`MetricsSnapshot`]
/// values.  Created once per meter and reused across calls.
pub struct MetricsExporter {
    actors_live: opentelemetry::metrics::Gauge<u64>,
    dlq_depth: opentelemetry::metrics::Gauge<u64>,
    mailbox_depth: opentelemetry::metrics::Gauge<u64>,
    scheduler_total: opentelemetry::metrics::Counter<u64>,
    scheduler_local: opentelemetry::metrics::Counter<u64>,
    scheduler_global: opentelemetry::metrics::Counter<u64>,
    scheduler_stolen: opentelemetry::metrics::Counter<u64>,
    scheduler_steal_attempts: opentelemetry::metrics::Counter<u64>,
    scheduler_steal_successes: opentelemetry::metrics::Counter<u64>,
    scheduler_empty_polls: opentelemetry::metrics::Counter<u64>,
    gc_objects_allocated: opentelemetry::metrics::Counter<u64>,
    gc_objects_freed: opentelemetry::metrics::Counter<u64>,
    gc_bytes_allocated: opentelemetry::metrics::Counter<u64>,
    gc_bytes_freed: opentelemetry::metrics::Counter<u64>,
    gc_cycles_detected: opentelemetry::metrics::Counter<u64>,
    resolver_local: opentelemetry::metrics::Counter<u64>,
    resolver_remote: opentelemetry::metrics::Counter<u64>,
    resolver_failed: opentelemetry::metrics::Counter<u64>,
    resolver_cache_hits: opentelemetry::metrics::Counter<u64>,
    resolver_cache_misses: opentelemetry::metrics::Counter<u64>,
    supervisors_total: opentelemetry::metrics::Gauge<u64>,
    supervisor_children: opentelemetry::metrics::Gauge<u64>,
    crdt_entries: opentelemetry::metrics::Gauge<u64>,
    crdt_ops_synced: opentelemetry::metrics::Gauge<u64>,
    crdt_unsynced_deltas: opentelemetry::metrics::Gauge<u64>,
    /// Previous snapshot, used to compute deltas for counter metrics.
    prev: parking_lot::Mutex<Option<crate::runtime::MetricsSnapshot>>,
}

impl MetricsExporter {
    /// Create instruments under the given `meter`.
    pub fn new(meter: &Meter) -> Self {
        MetricsExporter {
            actors_live: meter.u64_gauge("nulang.actors.live").init(),
            dlq_depth: meter.u64_gauge("nulang.dlq.depth").init(),
            mailbox_depth: meter.u64_gauge("nulang.actor.mailbox.depth").init(),
            scheduler_total: meter.u64_counter("nulang.scheduler.tasks.total").init(),
            scheduler_local: meter.u64_counter("nulang.scheduler.tasks.local").init(),
            scheduler_global: meter.u64_counter("nulang.scheduler.tasks.global").init(),
            scheduler_stolen: meter.u64_counter("nulang.scheduler.tasks.stolen").init(),
            scheduler_steal_attempts: meter.u64_counter("nulang.scheduler.steal.attempts").init(),
            scheduler_steal_successes: meter.u64_counter("nulang.scheduler.steal.successes").init(),
            scheduler_empty_polls: meter.u64_counter("nulang.scheduler.empty_polls").init(),
            gc_objects_allocated: meter.u64_counter("nulang.gc.objects.allocated").init(),
            gc_objects_freed: meter.u64_counter("nulang.gc.objects.freed").init(),
            gc_bytes_allocated: meter.u64_counter("nulang.gc.bytes.allocated").init(),
            gc_bytes_freed: meter.u64_counter("nulang.gc.bytes.freed").init(),
            gc_cycles_detected: meter.u64_counter("nulang.gc.cycles.detected").init(),
            resolver_local: meter.u64_counter("nulang.resolver.local").init(),
            resolver_remote: meter.u64_counter("nulang.resolver.remote").init(),
            resolver_failed: meter.u64_counter("nulang.resolver.failed").init(),
            resolver_cache_hits: meter.u64_counter("nulang.resolver.cache.hits").init(),
            resolver_cache_misses: meter.u64_counter("nulang.resolver.cache.misses").init(),
            supervisors_total: meter.u64_gauge("nulang.supervisors.total").init(),
            supervisor_children: meter.u64_gauge("nulang.supervisor.children").init(),
            crdt_entries: meter.u64_gauge("nulang.crdt.entries").init(),
            crdt_ops_synced: meter.u64_gauge("nulang.crdt.ops.synced").init(),
            crdt_unsynced_deltas: meter.u64_gauge("nulang.crdt.unsynced.deltas").init(),
            prev: parking_lot::Mutex::new(None),
        }
    }

    /// Publish a metrics snapshot to OTLP.  Gauges are set to the absolute
    /// value; counters are incremented by the delta from the previous snapshot.
    pub fn publish(&self, snap: &crate::runtime::MetricsSnapshot) {
        let prev = self.prev.lock();
        let prev_ref = prev.as_ref();

        self.actors_live.record(snap.actors_live, &[]);
        self.dlq_depth.record(snap.dlq_depth, &[]);

        for m in &snap.actors_mailboxes {
            self.mailbox_depth.record(
                m.depth as u64,
                &[opentelemetry::KeyValue::new("actor_id", m.actor_id as i64)],
            );
        }

        let s = &snap.scheduler;
        let p_s = prev_ref.map(|p| &p.scheduler);
        add_delta(
            &self.scheduler_total,
            s.total_tasks_processed,
            p_s.map(|p| p.total_tasks_processed),
        );
        add_delta(
            &self.scheduler_local,
            s.tasks_from_local_queue,
            p_s.map(|p| p.tasks_from_local_queue),
        );
        add_delta(
            &self.scheduler_global,
            s.tasks_from_global_queue,
            p_s.map(|p| p.tasks_from_global_queue),
        );
        add_delta(
            &self.scheduler_stolen,
            s.tasks_from_steal,
            p_s.map(|p| p.tasks_from_steal),
        );
        add_delta(
            &self.scheduler_steal_attempts,
            s.steal_attempts,
            p_s.map(|p| p.steal_attempts),
        );
        add_delta(
            &self.scheduler_steal_successes,
            s.steal_successes,
            p_s.map(|p| p.steal_successes),
        );
        add_delta(
            &self.scheduler_empty_polls,
            s.empty_polls,
            p_s.map(|p| p.empty_polls),
        );

        let g = &snap.gc;
        let p_g = prev_ref.map(|p| &p.gc);
        add_delta(
            &self.gc_objects_allocated,
            g.objects_allocated,
            p_g.map(|p| p.objects_allocated),
        );
        add_delta(
            &self.gc_objects_freed,
            g.objects_freed,
            p_g.map(|p| p.objects_freed),
        );
        add_delta(
            &self.gc_bytes_allocated,
            g.bytes_allocated,
            p_g.map(|p| p.bytes_allocated),
        );
        add_delta(
            &self.gc_bytes_freed,
            g.bytes_freed,
            p_g.map(|p| p.bytes_freed),
        );
        add_delta(
            &self.gc_cycles_detected,
            g.cycles_detected,
            p_g.map(|p| p.cycles_detected),
        );

        let r = &snap.resolver;
        let p_r = prev_ref.map(|p| &p.resolver);
        add_delta(
            &self.resolver_local,
            r.local_resolves,
            p_r.map(|p| p.local_resolves),
        );
        add_delta(
            &self.resolver_remote,
            r.remote_resolves,
            p_r.map(|p| p.remote_resolves),
        );
        add_delta(
            &self.resolver_failed,
            r.failed_resolves,
            p_r.map(|p| p.failed_resolves),
        );
        add_delta(
            &self.resolver_cache_hits,
            r.cache_hits,
            p_r.map(|p| p.cache_hits),
        );
        add_delta(
            &self.resolver_cache_misses,
            r.cache_misses,
            p_r.map(|p| p.cache_misses),
        );

        self.supervisors_total
            .record(snap.supervisors.len() as u64, &[]);
        for s in &snap.supervisors {
            self.supervisor_children.record(
                s.children.len() as u64,
                &[opentelemetry::KeyValue::new("supervisor_id", s.id as i64)],
            );
        }
        let c = &snap.crdt;
        self.crdt_entries.record(c.entries as u64, &[]);
        self.crdt_ops_synced.record(c.ops_synced, &[]);
        self.crdt_unsynced_deltas
            .record(c.unsynced_deltas as u64, &[]);

        drop(prev);
        *self.prev.lock() = Some(snap.clone());
    }
}

/// Helper: add `current - previous` to a counter, or `current` if no previous.
fn add_delta(counter: &opentelemetry::metrics::Counter<u64>, current: u64, previous: Option<u64>) {
    let delta = current.saturating_sub(previous.unwrap_or(0));
    if delta > 0 {
        counter.add(delta, &[]);
    }
}

/// Cached global [`MetricsExporter`] so instruments are created only once.
static OTLP_EXPORTER: std::sync::LazyLock<MetricsExporter> = std::sync::LazyLock::new(|| {
    MetricsExporter::new(&opentelemetry::global::meter("nulang-runtime"))
});

/// Publish a [`MetricsSnapshot`] to OTLP.  The exporter is lazily
/// initialised on the first call using the global meter provider
/// (which must have been set up via [`init_meter`]).  No-op if the
/// global meter provider has not been configured.
pub fn publish_otlp_metrics(snap: &crate::runtime::MetricsSnapshot) {
    OTLP_EXPORTER.publish(snap);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `init_tracer` must be idempotent.
    #[test]
    fn test_init_tracer_idempotent() {
        let url = "http://localhost:4318/v1/traces";
        let name = "nulang-test-tracer";
        // First call may fail if no collector is running, but idempotency
        // should still hold when the guard is already set.
        let r1 = init_tracer(url, name);
        let r2 = init_tracer(url, name);
        // Both calls should return the same result (Ok or Err).
        assert_eq!(r1.is_ok(), r2.is_ok());
    }

    /// `init_meter` must be idempotent.
    #[test]
    fn test_init_meter_idempotent() {
        let url = "http://localhost:4318/v1/metrics";
        let name = "nulang-test-meter";
        // The metrics pipeline requires a Tokio runtime for its background
        // periodic reader. Create one for the test.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (r1, r2) = rt.block_on(async {
            let r1 = init_meter(url, name);
            let r2 = init_meter(url, name);
            (r1, r2)
        });
        assert_eq!(r1.is_ok(), r2.is_ok());
    }
}
