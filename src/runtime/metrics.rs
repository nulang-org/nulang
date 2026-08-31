//! Prometheus-format metrics export for VictoriaMetrics / Prometheus scraping.
//!
//! A lightweight TCP server that serves `GET /metrics` in Prometheus
//! exposition format.  No external dependencies — pure `std::net::TcpListener`
//! on a background thread.  The scheduler thread periodically calls
//! [`Runtime::publish_metrics`] to push the latest snapshot into a shared
//! buffer; the server thread serves whichever snapshot it last received.
use std::net::TcpListener;

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::MetricsSnapshot;

/// Start a background Prometheus-format metrics server on `port`.
///
/// Returns a handle and a shared buffer.  The caller should periodically
/// call `publish(snapshot)` to push the latest snapshot; the server
/// thread serves the most recently published snapshot.
pub struct MetricsServer {
    #[allow(dead_code)]
    handle: JoinHandle<()>,
    buffer: Arc<Mutex<String>>,
}

impl MetricsServer {
    /// Bind and start serving on `0.0.0.0:<port>`.
    pub fn start(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        listener.set_nonblocking(false)?;
        let buffer = Arc::new(Mutex::new(String::from(
            "# Nulang metrics server starting up — no snapshot yet\n",
        )));
        let buf = buffer.clone();

        let handle = thread::Builder::new()
            .name("nulang-metrics".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(mut s) => {
                            let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
                            let body = buf.lock().unwrap().clone();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = s.write_all(response.as_bytes());
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(MetricsServer { handle, buffer })
    }

    /// Publish a new snapshot.  Thread-safe — call from the scheduler thread.
    pub fn publish(&self, text: String) {
        *self.buffer.lock().unwrap() = text;
    }
}

impl MetricsSnapshot {
    /// Format this snapshot as Prometheus exposition text.
    ///
    /// VictoriaMetrics and Prometheus both natively scrape this format.
    pub fn to_prometheus_text(&self) -> String {
        let mut out = String::new();

        // Gauge: live actor count
        out.push_str("# HELP nulang_actors_live Number of living actors\n");
        out.push_str("# TYPE nulang_actors_live gauge\n");
        out.push_str(&format!("nulang_actors_live {}\n", self.actors_live));

        // Gauge: DLQ depth
        out.push_str("# HELP nulang_dlq_depth Dead-letter queue depth\n");
        out.push_str("# TYPE nulang_dlq_depth gauge\n");
        out.push_str(&format!("nulang_dlq_depth {}\n", self.dlq_depth));

        // Gauge: per-actor mailbox depths (top 50 by depth)
        out.push_str("# HELP nulang_actor_mailbox_depth Per-actor mailbox depth\n");
        out.push_str("# TYPE nulang_actor_mailbox_depth gauge\n");
        let mut sorted: Vec<_> = self.actors_mailboxes.clone();
        sorted.sort_by_key(|m| -(m.depth as i64));
        for m in sorted.iter().take(50) {
            out.push_str(&format!(
                "nulang_actor_mailbox_depth{{actor_id=\"{}\"}} {}\n",
                m.actor_id, m.depth
            ));
        }

        // Scheduler counters
        let s = &self.scheduler;
        macro_rules! counter {
            ($name:expr, $help:expr, $val:expr) => {
                out.push_str(concat!("# HELP ", $name, " ", $help, "\n"));
                out.push_str(concat!("# TYPE ", $name, " counter\n"));
                out.push_str(&format!(concat!($name, " {}\n"), $val));
            };
        }
        counter!(
            "nulang_scheduler_tasks_total",
            "Total tasks processed",
            s.total_tasks_processed
        );
        counter!(
            "nulang_scheduler_tasks_local",
            "Tasks from local queue",
            s.tasks_from_local_queue
        );
        counter!(
            "nulang_scheduler_tasks_global",
            "Tasks from global queue",
            s.tasks_from_global_queue
        );
        counter!(
            "nulang_scheduler_tasks_stolen",
            "Tasks stolen from other workers",
            s.tasks_from_steal
        );
        counter!(
            "nulang_scheduler_steal_attempts",
            "Steal attempts",
            s.steal_attempts
        );
        counter!(
            "nulang_scheduler_steal_successes",
            "Successful steals",
            s.steal_successes
        );
        counter!(
            "nulang_scheduler_empty_polls",
            "Empty polls (no work found)",
            s.empty_polls
        );

        // GC counters
        let g = &self.gc;
        counter!(
            "nulang_gc_objects_allocated",
            "Objects allocated",
            g.objects_allocated
        );
        counter!("nulang_gc_objects_freed", "Objects freed", g.objects_freed);
        counter!(
            "nulang_gc_bytes_allocated",
            "Bytes allocated",
            g.bytes_allocated
        );
        counter!("nulang_gc_bytes_freed", "Bytes freed", g.bytes_freed);
        counter!(
            "nulang_gc_cycles_detected",
            "ORCA cycles detected",
            g.cycles_detected
        );

        // Resolver counters
        let r = &self.resolver;
        counter!(
            "nulang_resolver_local_resolves",
            "Local address resolutions",
            r.local_resolves
        );
        counter!(
            "nulang_resolver_remote_resolves",
            "Remote address resolutions",
            r.remote_resolves
        );
        counter!(
            "nulang_resolver_failed_resolves",
            "Failed resolutions",
            r.failed_resolves
        );
        counter!(
            "nulang_resolver_cache_hits",
            "Remote actor cache hits",
            r.cache_hits
        );
        counter!(
            "nulang_resolver_cache_misses",
            "Remote actor cache misses",
            r.cache_misses
        );

        // Supervision topology: total supervisors + per-supervisor child count.
        out.push_str("# HELP nulang_supervisors_total Number of supervisors\n");
        out.push_str("# TYPE nulang_supervisors_total gauge\n");
        out.push_str(&format!(
            "nulang_supervisors_total {}\n",
            self.supervisors.len()
        ));
        out.push_str("# HELP nulang_supervisor_children Supervisor child actor count\n");
        out.push_str("# TYPE nulang_supervisor_children gauge\n");
        for sup in &self.supervisors {
            out.push_str(&format!(
                "nulang_supervisor_children{{supervisor_id=\"{}\"}} {}\n",
                sup.id,
                sup.children.len()
            ));
        }

        // CRDT replication state.
        out.push_str("# HELP nulang_crdt_entries Number of live CRDT entries\n");
        out.push_str("# TYPE nulang_crdt_entries gauge\n");
        out.push_str(&format!("nulang_crdt_entries {}\n", self.crdt.entries));
        out.push_str("# HELP nulang_crdt_ops_synced Total CRDT ops shipped\n");
        out.push_str("# TYPE nulang_crdt_ops_synced counter\n");
        out.push_str(&format!(
            "nulang_crdt_ops_synced {}\n",
            self.crdt.ops_synced
        ));
        out.push_str("# HELP nulang_crdt_unsynced_deltas CRDT entries with unsynced changes\n");
        out.push_str("# TYPE nulang_crdt_unsynced_deltas gauge\n");
        out.push_str(&format!(
            "nulang_crdt_unsynced_deltas {}\n",
            self.crdt.unsynced_deltas
        ));

        out
    }

    /// Render the runtime topology as ASCII text: a summary line plus the
    /// supervision tree (supervisors nested by parent link, supervised
    /// actors as leaves). Used for a terminal topology view.
    pub fn render_topology_text(&self) -> String {
        let sup_by_id: HashMap<u64, &super::SupervisorMetric> =
            self.supervisors.iter().map(|s| (s.id, s)).collect();

        let mut out = String::new();
        out.push_str(&format!(
            "runtime: actors_live={} supervisors={} dlq_depth={} crdt_entries={} (unsynced={})\n",
            self.actors_live,
            self.supervisors.len(),
            self.dlq_depth,
            self.crdt.entries,
            self.crdt.unsynced_deltas
        ));

        // Roots are supervisors not referenced as a child by any other
        // supervisor. Building from child relationships (rather than the
        // `parent` field) is robust: `supervise_child` records the child in
        // the parent's `children`, but does not always set the child
        // supervisor's `parent`.
        let child_sup_ids: std::collections::HashSet<u64> = self
            .supervisors
            .iter()
            .flat_map(|s| s.children.iter().map(|c| c.actor_id))
            .filter(|id| sup_by_id.contains_key(id))
            .collect();
        let roots: Vec<&super::SupervisorMetric> = self
            .supervisors
            .iter()
            .filter(|s| !child_sup_ids.contains(&s.id))
            .collect();

        if roots.is_empty() {
            out.push_str("  (no supervisors)\n");
        }
        for root in roots {
            Self::render_supervisor_node(root, 0, &sup_by_id, &mut out);
        }
        out
    }

    fn render_supervisor_node(
        sup: &super::SupervisorMetric,
        depth: usize,
        sup_by_id: &HashMap<u64, &super::SupervisorMetric>,
        out: &mut String,
    ) {
        let indent = "  ".repeat(depth);
        out.push_str(&format!(
            "{}supervisor {} [{}]\n",
            indent, sup.name, sup.strategy
        ));
        for child in &sup.children {
            match sup_by_id.get(&child.actor_id) {
                Some(child_sup) => {
                    Self::render_supervisor_node(child_sup, depth + 1, sup_by_id, out)
                }
                None => {
                    out.push_str(&format!(
                        "{}  actor {} ({})\n",
                        "  ".repeat(depth + 1),
                        child.actor_id,
                        child.spec_id
                    ));
                }
            }
        }
    }
}
