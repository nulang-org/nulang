#[cfg(unix)]
mod unix {
    use nulang::runtime::{
        cache::{CacheLimits, CacheStore},
        cache_cluster::{CacheAdvertisedEndpoint, CacheEndpointMap},
        cache_dispatch::{CacheDispatchChannels, CacheDispatcher},
        cache_routing::CacheSlotMap,
        cache_server::{
            AutomaticCheckpointConfig, CacheDurableConfig, CacheMutualTls,
            CacheOperationalSnapshot, CacheServerClock, CacheServerConfig, CacheShardServer,
            TLS_RESERVATION_BYTES,
        },
        cache_wal::{WalLimits, WalSync, MAX_CLOCK_RESERVATION_MS},
    };
    use std::{collections::HashSet, io::Write, net::SocketAddr, path::PathBuf};

    const HELP: &str =
        "nulang-fabric [--bind 127.0.0.1:6380] [--mode memory|buffered|sync] [--journal PATH]
    --max-entries N --max-arena-bytes N --max-connections N --max-connections-per-peer N
    --max-connection-bytes N --max-input-bytes N --max-output-bytes N --max-pipeline-depth N
    --max-record-bytes N --max-journal-bytes N
    --group-commit 0|1 (default 0; sync mode only)
    --clock-reservation-ms 0..1000 (default 0; may delay restart)
    --checkpoint-percent 0..99 (default 70; 0 disables)
    --checkpoint-min-savings-percent 1..99 (default 10)
    --checkpoint-retry-ms N (default 5000)
    --shutdown-drain-ms 0..60000 (default 5000)
    --metrics-bind 127.0.0.1:PORT (optional local HTTP metrics and health)
    --tls-ca PATH --tls-cert PATH --tls-key PATH (all required for mutual TLS)

Single-shard RESP server. Plaintext is loopback-only; remote binds require mutual TLS.
Durable modes require a private journal path. Memory mode is the default.";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Memory,
        Buffered,
        Sync,
    }

    #[derive(Debug)]
    struct Config {
        bind: SocketAddr,
        mode: Mode,
        journal: Option<PathBuf>,
        cache: CacheLimits,
        server: CacheServerConfig,
        wal: WalLimits,
        checkpoint: AutomaticCheckpointConfig,
        shutdown_drain_ms: u64,
        group_commit: bool,
        clock_reservation_ms: u64,
        metrics_bind: Option<SocketAddr>,
        tls_ca: Option<PathBuf>,
        tls_cert: Option<PathBuf>,
        tls_key: Option<PathBuf>,
    }

    fn positive<T: std::str::FromStr + PartialEq + Default>(
        value: &str,
        flag: &str,
    ) -> Result<T, String> {
        let number = value.parse::<T>().map_err(|_| format!("invalid {flag}"))?;
        if number == T::default() {
            return Err(format!("{flag} must be positive"));
        }
        Ok(number)
    }

    impl Config {
        fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
            let mut config = Self {
                bind: "127.0.0.1:6380".parse().unwrap(),
                mode: Mode::Memory,
                journal: None,
                cache: CacheLimits::default(),
                server: CacheServerConfig::default(),
                wal: WalLimits::default(),
                checkpoint: AutomaticCheckpointConfig::default(),
                shutdown_drain_ms: 5000,
                group_commit: false,
                clock_reservation_ms: 0,
                metrics_bind: None,
                tls_ca: None,
                tls_cert: None,
                tls_key: None,
            };
            let mut seen = HashSet::new();
            let mut args = args.into_iter();
            while let Some(flag) = args.next() {
                if !seen.insert(flag.clone()) {
                    return Err(format!("duplicate option {flag}"));
                }
                let value = args
                    .next()
                    .ok_or_else(|| format!("missing value for {flag}"))?;
                match flag.as_str() {
                    "--bind" => {
                        config.bind = value
                            .parse()
                            .map_err(|_| "invalid numeric socket address".to_string())?
                    }
                    "--mode" => {
                        config.mode = match value.as_str() {
                            "memory" => Mode::Memory,
                            "buffered" => Mode::Buffered,
                            "sync" => Mode::Sync,
                            _ => return Err("mode must be memory, buffered or sync".into()),
                        }
                    }
                    "--journal" => {
                        if value.is_empty() {
                            return Err("empty journal path".into());
                        }
                        config.journal = Some(PathBuf::from(value));
                    }
                    "--max-entries" => config.cache.max_entries = positive(&value, &flag)?,
                    "--max-arena-bytes" => config.cache.max_arena_bytes = positive(&value, &flag)?,
                    "--max-pipeline-depth" => {
                        config.server.max_pipeline_depth = positive(&value, &flag)?
                    }
                    "--max-connections" => config.server.max_connections = positive(&value, &flag)?,
                    "--max-connections-per-peer" => {
                        config.server.max_connections_per_peer = positive(&value, &flag)?
                    }
                    "--max-connection-bytes" => {
                        config.server.max_total_connection_bytes = positive(&value, &flag)?
                    }
                    "--max-input-bytes" => {
                        config.server.max_input_buffer = positive(&value, &flag)?
                    }
                    "--max-output-bytes" => {
                        config.server.max_output_buffer = positive(&value, &flag)?
                    }
                    "--max-record-bytes" => config.wal.max_record_bytes = positive(&value, &flag)?,
                    "--max-journal-bytes" => config.wal.max_file_bytes = positive(&value, &flag)?,
                    "--checkpoint-percent" => {
                        config.checkpoint.trigger_percent =
                            value.parse().map_err(|_| "invalid checkpoint percent")?
                    }
                    "--checkpoint-min-savings-percent" => {
                        config.checkpoint.min_savings_percent = positive(&value, &flag)?
                    }
                    "--checkpoint-retry-ms" => {
                        config.checkpoint.retry_cooldown_ms = positive(&value, &flag)?
                    }
                    "--shutdown-drain-ms" => {
                        config.shutdown_drain_ms = value
                            .parse()
                            .map_err(|_| "invalid shutdown drain interval")?;
                        if config.shutdown_drain_ms > 60_000 {
                            return Err("shutdown drain interval exceeds 60000 ms".into());
                        }
                    }

                    "--group-commit" => {
                        config.group_commit = match value.as_str() {
                            "0" => false,
                            "1" => true,
                            _ => return Err("group commit must be 0 or 1".into()),
                        };
                    }
                    "--clock-reservation-ms" => {
                        config.clock_reservation_ms =
                            value.parse().map_err(|_| "invalid clock reservation")?;
                        if config.clock_reservation_ms > MAX_CLOCK_RESERVATION_MS {
                            return Err("clock reservation exceeds 1000 ms".into());
                        }
                    }
                    "--metrics-bind" => {
                        let address: SocketAddr = value
                            .parse()
                            .map_err(|_| "invalid numeric metrics address")?;
                        if !address.ip().is_loopback() || address.port() == 0 {
                            return Err("metrics bind must be loopback with a nonzero port".into());
                        }
                        config.metrics_bind = Some(address);
                    }
                    "--tls-ca" => config.tls_ca = Some(PathBuf::from(value)),
                    "--tls-cert" => config.tls_cert = Some(PathBuf::from(value)),
                    "--tls-key" => config.tls_key = Some(PathBuf::from(value)),
                    _ => return Err(format!("unknown option {flag}")),
                }
            }
            let tls_count = [&config.tls_ca, &config.tls_cert, &config.tls_key]
                .iter()
                .filter(|p| p.is_some())
                .count();
            if tls_count != 0 && tls_count != 3 {
                return Err("all three TLS paths are required".into());
            }
            if tls_count == 3 && config.server.max_total_connection_bytes <= TLS_RESERVATION_BYTES {
                return Err(
                    "TLS requires a connection byte budget greater than 524288 bytes".into(),
                );
            }
            if config.bind.port() == 0 || (!config.bind.ip().is_loopback() && tls_count != 3) {
                return Err("non-loopback bind requires mutual TLS and a nonzero port".into());
            }
            if (config.mode == Mode::Memory) != config.journal.is_none() {
                return Err("a journal path is required exactly in buffered or sync mode".into());
            }
            if config.mode != Mode::Sync && seen.contains("--group-commit") {
                return Err("group commit requires sync mode".into());
            }
            if config.mode == Mode::Memory && seen.contains("--clock-reservation-ms") {
                return Err("clock reservation requires durable mode".into());
            }
            if config.mode == Mode::Memory
                && (seen.contains("--max-record-bytes") || seen.contains("--max-journal-bytes"))
            {
                return Err("journal limits require durable mode".into());
            }
            if config.wal.max_record_bytes > u32::MAX as usize || config.wal.max_file_bytes < 56 {
                return Err("invalid journal limits".into());
            }
            if config.checkpoint.trigger_percent > 99
                || !(1..=99).contains(&config.checkpoint.min_savings_percent)
            {
                return Err(
                    "checkpoint percentages must be below 100 (minimum savings must be positive)"
                        .into(),
                );
            }
            if config.mode == Mode::Memory
                && seen.iter().any(|flag| flag.starts_with("--checkpoint-"))
            {
                return Err("checkpoint options require durable mode".into());
            }
            Ok(config)
        }

        fn bind(self) -> Result<CacheShardServer, String> {
            // Parse and validate all security material before binding.
            let tls = match (&self.tls_ca, &self.tls_cert, &self.tls_key) {
                (Some(ca), Some(cert), Some(key)) => {
                    fn read(path: &std::path::Path) -> Result<Vec<u8>, String> {
                        use std::io::Read;
                        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
                        let mut bytes = Vec::new();
                        file.take(1024 * 1024 + 1)
                            .read_to_end(&mut bytes)
                            .map_err(|e| e.to_string())?;
                        if bytes.len() > 1024 * 1024 {
                            return Err("TLS PEM exceeds 1 MiB".into());
                        }
                        Ok(bytes)
                    }
                    Some(
                        CacheMutualTls::from_pem(&read(ca)?, &read(cert)?, &read(key)?)
                            .map_err(|e| e.to_string())?,
                    )
                }
                (None, None, None) => None,
                _ => return Err("incomplete mutual TLS configuration".into()),
            };

            let placement = CacheSlotMap::new_local(1, 1).map_err(|e| format!("{e:?}"))?;
            let owner = placement.owner_for_slot(0).ok_or("missing local owner")?;
            let (channels, mut inboxes) =
                CacheDispatchChannels::new(1, 32).map_err(|e| format!("{e:?}"))?;
            let mut endpoints = CacheEndpointMap::new();
            endpoints.insert(
                owner,
                CacheAdvertisedEndpoint::new(&self.bind.ip().to_string(), self.bind.port()),
            );
            let dispatcher = CacheDispatcher::new(1, 0, placement, channels)
                .map_err(|e| format!("{e:?}"))?
                .with_cluster_redirects(endpoints);
            let clock = CacheServerClock::new();
            let mut server = match self.mode {
                Mode::Memory => CacheShardServer::bind(
                    self.bind,
                    dispatcher,
                    inboxes.remove(0),
                    CacheStore::with_limits(self.cache),
                    self.server,
                    clock,
                ),
                Mode::Buffered | Mode::Sync => CacheShardServer::bind_durable(
                    self.bind,
                    dispatcher,
                    inboxes.remove(0),
                    CacheDurableConfig {
                        path: self.journal.ok_or("missing journal path")?,
                        cache_limits: self.cache,
                        wal_limits: self.wal,
                        sync: if self.mode == Mode::Sync {
                            WalSync::Data
                        } else {
                            WalSync::Buffered
                        },
                    },
                    self.server,
                    clock,
                ),
            }
            .map_err(|e| format!("bind/recovery failed: {e:?}"))?;
            if let Some(tls) = tls {
                server
                    .enable_mutual_tls(tls)
                    .map_err(|e| format!("{e:?}"))?;
            }
            if self.group_commit {
                server.enable_group_commit().map_err(|e| format!("{e:?}"))?;
            }
            if self.mode != Mode::Memory {
                server
                    .configure_clock_reservation(self.clock_reservation_ms)
                    .map_err(|e| format!("{e:?}"))?;
            }
            if self.mode != Mode::Memory && self.checkpoint.trigger_percent != 0 {
                server
                    .enable_automatic_checkpoint(self.checkpoint)
                    .map_err(|e| format!("{e:?}"))?;
            }
            Ok(server)
        }
    }

    mod metrics {
        use super::CacheOperationalSnapshot;
        use std::fmt::Write as _;
        use std::io::{self, Read, Write};
        use std::net::{SocketAddr, TcpListener, TcpStream};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        };
        use std::time::{Duration, Instant};

        const IO_DEADLINE: Duration = Duration::from_millis(250);
        const STALE_AFTER: Duration = Duration::from_secs(2);
        type Sample = (Instant, CacheOperationalSnapshot);

        pub(super) struct Monitor {
            latest: Arc<Mutex<Sample>>,
            stop: Arc<AtomicBool>,
            thread: Option<std::thread::JoinHandle<()>>,
        }

        impl Monitor {
            pub(super) fn start(
                address: SocketAddr,
                initial: CacheOperationalSnapshot,
            ) -> io::Result<Self> {
                if !address.ip().is_loopback() {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                let listener = TcpListener::bind(address)?;
                listener.set_nonblocking(true)?;
                let latest = Arc::new(Mutex::new((Instant::now(), initial)));
                let stop = Arc::new(AtomicBool::new(false));
                let samples = latest.clone();
                let stopped = stop.clone();
                let thread = std::thread::Builder::new()
                    .name("fabric-metrics".into())
                    .spawn(move || {
                        while !stopped.load(Ordering::Acquire) {
                            match listener.accept() {
                                Ok((stream, peer)) => {
                                    if peer.ip().is_loopback() {
                                        let _ = serve(stream, &samples);
                                    }
                                }
                                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                                // Includes descriptor pressure: back off instead of spin.
                                Err(_) => std::thread::sleep(Duration::from_millis(20)),
                            }
                        }
                    })?;
                Ok(Self {
                    latest,
                    stop,
                    thread: Some(thread),
                })
            }
            pub(super) fn publish(&self, sample: CacheOperationalSnapshot) {
                // The observer must not block the reactor on a scrape.
                if let Ok(mut latest) = self.latest.try_lock() {
                    *latest = (Instant::now(), sample);
                }
            }
        }

        impl Drop for Monitor {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Release);
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }

        fn format_metrics(sample: CacheOperationalSnapshot, fresh: bool) -> String {
            let mut body = String::with_capacity(2048);
            macro_rules! gauge {
                ($name:literal, $value:expr) => {
                    let _ = writeln!(body, concat!("fabric_", $name, " {}"), $value);
                };
            }
            let c = sample.counters;
            gauge!("snapshot_fresh", u8::from(fresh));
            gauge!("healthy", u8::from(fresh && sample.healthy));
            gauge!("ready", u8::from(fresh && sample.ready));
            gauge!("connections_active", sample.connections);
            gauge!("connections_accepted_total", c.accepted);
            gauge!("connections_rejected_limit_total", c.rejected_limit);
            gauge!("connections_rejected_peer_total", c.rejected_peer);
            gauge!(
                "connections_rejected_tls_budget_total",
                c.rejected_tls_budget
            );
            gauge!("accept_resource_errors_total", c.accept_resource_errors);
            gauge!("connection_drive_errors_total", c.connection_errors);
            gauge!("tls_handshake_timeouts_total", c.handshake_timeouts);
            gauge!("commands_dispatched_total", c.commands_dispatched);
            gauge!("polls_completed_total", c.polls_completed);
            gauge!("wal_groups_synced_total", c.wal_groups_synced);
            gauge!("wal_group_records_synced_total", c.wal_group_records_synced);
            gauge!("tls_reserved_bytes", sample.tls_reserved_bytes);
            gauge!(
                "connection_budget_used_bytes",
                sample.connection_budget_used_bytes
            );
            gauge!(
                "connection_budget_limit_bytes",
                sample.connection_budget_limit_bytes
            );
            gauge!("cache_entries", sample.cache.entries);
            gauge!(
                "cache_arena_reserved_bytes",
                sample.cache.arena_reserved_bytes
            );
            gauge!(
                "cache_retained_capacity_bytes",
                sample.cache.retained_capacity_bytes
            );
            gauge!("cache_hits_total", sample.cache_stats.hits);
            gauge!("cache_misses_total", sample.cache_stats.misses);
            gauge!("cache_expirations_total", sample.cache_stats.expirations);
            gauge!("durable", u8::from(sample.durable));
            gauge!("journal_bytes", sample.journal_bytes);
            gauge!("journal_limit_bytes", sample.journal_limit_bytes);
            gauge!("checkpoint_completed_total", sample.checkpoint_completed);
            gauge!("checkpoint_failed_total", sample.checkpoint_failed);
            gauge!("checkpoint_skipped_total", sample.checkpoint_skipped);
            gauge!(
                "checkpoint_reclaimed_bytes_total",
                sample.checkpoint_reclaimed_bytes
            );
            body
        }

        fn serve(mut stream: TcpStream, latest: &Mutex<Sample>) -> io::Result<()> {
            stream.set_nonblocking(false)?;
            let start = Instant::now();
            let mut request = [0u8; 1024];
            let mut used = 0;
            let mut valid = false;
            while used < request.len() {
                let remaining = IO_DEADLINE.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    return Ok(());
                }
                stream.set_read_timeout(Some(remaining))?;
                let n = stream.read(&mut request[used..])?;
                if n == 0 {
                    return Ok(());
                }
                used += n;
                if request[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    valid = true;
                    break;
                }
            }
            let first_line = request[..used]
                .split(|b| *b == b'\n')
                .next()
                .unwrap_or_default();
            let (sample_at, sample) = *latest
                .lock()
                .map_err(|_| io::Error::other("metrics snapshot unavailable"))?;
            let fresh = sample_at.elapsed() <= STALE_AFTER;
            let (status, content_type, body) = if !valid {
                ("400 Bad Request", "text/plain", "bad request\n".to_string())
            } else {
                match first_line {
                    b"GET /metrics HTTP/1.1\r" | b"GET /metrics HTTP/1.0\r" => (
                        "200 OK",
                        "text/plain; version=0.0.4; charset=utf-8",
                        format_metrics(sample, fresh),
                    ),
                    b"GET /healthz HTTP/1.1\r" | b"GET /healthz HTTP/1.0\r" => {
                        health(fresh && sample.healthy)
                    }
                    b"GET /readyz HTTP/1.1\r" | b"GET /readyz HTTP/1.0\r" => {
                        health(fresh && sample.ready)
                    }
                    _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
                }
            };
            let remaining = IO_DEADLINE.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(());
            }
            stream.set_write_timeout(Some(remaining))?;
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}", body.len());
            let mut remaining_bytes = response.as_bytes();
            while !remaining_bytes.is_empty() {
                let remaining = IO_DEADLINE.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    return Ok(());
                }
                stream.set_write_timeout(Some(remaining))?;
                match stream.write(remaining_bytes) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(written) => remaining_bytes = &remaining_bytes[written..],
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        fn health(ok: bool) -> (&'static str, &'static str, String) {
            if ok {
                ("200 OK", "text/plain", "ok\n".into())
            } else {
                (
                    "503 Service Unavailable",
                    "text/plain",
                    "unavailable\n".into(),
                )
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use nulang::runtime::cache::{CacheMemoryStats, CacheStats};
            use nulang::runtime::cache_server::CacheOperationalCounters;
            fn sample() -> CacheOperationalSnapshot {
                CacheOperationalSnapshot {
                    healthy: true,
                    ready: true,
                    connections: 0,
                    tls_reserved_bytes: 0,
                    connection_budget_used_bytes: 0,
                    connection_budget_limit_bytes: 1024,
                    cache: CacheMemoryStats {
                        entries: 0,
                        index_capacity: 0,
                        arena_reserved_bytes: 0,
                        retained_capacity_bytes: 0,
                        reusable_slots: 0,
                    },
                    cache_stats: CacheStats::default(),
                    counters: CacheOperationalCounters::default(),
                    durable: false,
                    journal_bytes: 0,
                    journal_limit_bytes: 0,
                    checkpoint_completed: 0,
                    checkpoint_failed: 0,
                    checkpoint_skipped: 0,
                    checkpoint_reclaimed_bytes: 0,
                }
            }

            fn request_at(at: Instant, sample: CacheOperationalSnapshot, path: &str) -> String {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let address = listener.local_addr().unwrap();
                let thread = std::thread::spawn(move || {
                    let (socket, _) = listener.accept().unwrap();
                    serve(socket, &Mutex::new((at, sample))).unwrap();
                });
                let mut client = TcpStream::connect(address).unwrap();
                client
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                write!(client, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
                let mut response = String::new();
                client.read_to_string(&mut response).unwrap();
                thread.join().unwrap();
                response
            }

            #[test]
            fn stale_and_degraded_snapshots_fail_health_or_readiness() {
                let stale = Instant::now() - Duration::from_secs(3);
                assert!(request_at(stale, sample(), "/healthz").starts_with("HTTP/1.1 503"));
                assert!(request_at(stale, sample(), "/readyz").starts_with("HTTP/1.1 503"));
                let metrics = request_at(stale, sample(), "/metrics");
                assert!(metrics.contains("fabric_snapshot_fresh 0\n"));
                let mut degraded = sample();
                degraded.ready = false;
                assert!(
                    request_at(Instant::now(), degraded, "/healthz").starts_with("HTTP/1.1 200")
                );
                assert!(request_at(Instant::now(), degraded, "/readyz").starts_with("HTTP/1.1 503"));
                degraded.healthy = false;
                assert!(
                    request_at(Instant::now(), degraded, "/healthz").starts_with("HTTP/1.1 503")
                );
            }
        }
    }

    // Block termination signals before opening the journal or starting any
    // thread. A dedicated sigwait thread handles them in ordinary Rust
    // context, so no allocation, locks, or wake calls occur in a handler.
    fn block_termination_signals() -> Result<libc::sigset_t, String> {
        // SAFETY: initialize a POSIX signal set before using it.
        unsafe {
            let mut signals = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            if libc::sigemptyset(signals.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let mut signals = signals.assume_init();
            if libc::sigaddset(&mut signals, libc::SIGTERM) != 0
                || libc::sigaddset(&mut signals, libc::SIGINT) != 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let result = libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
            if result != 0 {
                return Err(std::io::Error::from_raw_os_error(result).to_string());
            }
            Ok(signals)
        }
    }

    pub fn run() -> Result<(), String> {
        let args = std::env::args_os()
            .skip(1)
            .map(|v| {
                v.into_string()
                    .map_err(|_| "arguments must be valid UTF-8".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if args == ["--help"] || args == ["-h"] {
            println!("{HELP}");
            return Ok(());
        }
        let config = Config::parse(args)?;
        let mode = config.mode;
        let timeout = std::time::Duration::from_millis(config.shutdown_drain_ms);
        let metrics_address = config.metrics_bind;
        let signals = block_termination_signals()?;
        let mut server = config.bind()?;
        let monitor = metrics_address
            .map(|address| metrics::Monitor::start(address, server.operational_snapshot()))
            .transpose()
            .map_err(|e| format!("metrics bind failed: {e}"))?;
        let control = server.control();
        let signal_error = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let thread_error = signal_error.clone();
        std::thread::Builder::new()
            .name("fabric-signals".into())
            .spawn(move || {
                let mut signal = 0;
                // SAFETY: signals is initialized and blocked in this thread;
                // sigwait synchronously consumes one of the termination signals.
                let result = unsafe { libc::sigwait(&signals, &mut signal) };
                thread_error.store(result, std::sync::atomic::Ordering::Release);
                control.shutdown();
            })
            .map_err(|e| e.to_string())?;

        println!(
            "READY {} {mode:?}",
            server.local_addr().map_err(|e| e.to_string())?
        );
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        if let Some(monitor) = &monitor {
            server.run_observed(timeout, |snapshot| monitor.publish(snapshot))
        } else {
            server.run_with_shutdown_timeout(timeout)
        }
        .map_err(|e| format!("reactor stopped: {e:?}"))?;
        let result = signal_error.load(std::sync::atomic::Ordering::Acquire);
        if result != 0 {
            return Err(std::io::Error::from_raw_os_error(result).to_string());
        }
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        fn parse(args: &[&str]) -> Result<Config, String> {
            Config::parse(args.iter().map(|s| s.to_string()))
        }

        #[test]
        fn loopback_memory_defaults_and_explicit_durability() {
            let defaults = parse(&[]).unwrap();
            assert_eq!(defaults.bind.to_string(), "127.0.0.1:6380");
            assert_eq!(defaults.mode, Mode::Memory);
            let sync = parse(&[
                "--mode",
                "sync",
                "--journal",
                "/tmp/private/cache.wal",
                "--bind",
                "[::1]:6381",
                "--max-entries",
                "200",
                "--max-arena-bytes",
                "4096",
            ])
            .unwrap();
            assert_eq!(sync.mode, Mode::Sync);
            assert_eq!(sync.cache.max_entries, 200);
            assert_eq!(sync.cache.max_arena_bytes, 4096);
        }

        #[test]
        fn checkpoint_options_are_durable_only_and_validated() {
            let config = parse(&["--mode", "buffered", "--journal", "cache.wal"]).unwrap();
            assert_eq!(config.checkpoint.trigger_percent, 70);
            let disabled = parse(&[
                "--mode",
                "sync",
                "--journal",
                "cache.wal",
                "--checkpoint-percent",
                "0",
            ])
            .unwrap();
            assert_eq!(disabled.checkpoint.trigger_percent, 0);
            for args in [
                vec!["--checkpoint-percent", "70"],
                vec![
                    "--mode",
                    "sync",
                    "--journal",
                    "cache.wal",
                    "--checkpoint-percent",
                    "100",
                ],
                vec![
                    "--mode",
                    "sync",
                    "--journal",
                    "cache.wal",
                    "--checkpoint-min-savings-percent",
                    "0",
                ],
                vec![
                    "--mode",
                    "sync",
                    "--journal",
                    "cache.wal",
                    "--checkpoint-retry-ms",
                    "0",
                ],
            ] {
                assert!(parse(&args).is_err(), "{args:?}");
            }
        }

        #[test]
        fn shutdown_drain_duration_is_bounded() {
            assert_eq!(parse(&[]).unwrap().shutdown_drain_ms, 5000);
            assert_eq!(
                parse(&["--shutdown-drain-ms", "0"])
                    .unwrap()
                    .shutdown_drain_ms,
                0
            );
            assert_eq!(
                parse(&["--shutdown-drain-ms", "60000"])
                    .unwrap()
                    .shutdown_drain_ms,
                60000
            );
            assert!(parse(&["--shutdown-drain-ms", "60001"]).is_err());
        }

        #[test]
        fn remote_binds_require_complete_mutual_tls_configuration() {
            assert!(parse(&["--bind", "0.0.0.0:6380"]).is_err());
            assert!(parse(&["--tls-cert", "server.pem"]).is_err());
            assert!(parse(&[
                "--bind",
                "0.0.0.0:6380",
                "--tls-ca",
                "ca.pem",
                "--tls-cert",
                "server.pem",
                "--tls-key",
                "server.key"
            ])
            .is_ok());
            assert!(parse(&[
                "--tls-ca",
                "missing-ca",
                "--tls-cert",
                "missing-cert",
                "--tls-key",
                "missing-key"
            ])
            .unwrap()
            .bind()
            .is_err());
        }

        #[test]
        fn tls_budget_requires_headroom_but_plaintext_can_use_small_budget() {
            assert!(parse(&["--max-connection-bytes", "4096"]).is_ok());
            assert!(parse(&[
                "--tls-ca",
                "ca.pem",
                "--tls-cert",
                "server.pem",
                "--tls-key",
                "server.key",
                "--max-connection-bytes",
                "524288"
            ])
            .is_err());
            assert!(parse(&[
                "--tls-ca",
                "ca.pem",
                "--tls-cert",
                "server.pem",
                "--tls-key",
                "server.key",
                "--max-connection-bytes",
                "1048576"
            ])
            .is_ok());
        }

        #[test]
        fn peer_limit_is_positive_and_configurable() {
            assert_eq!(parse(&[]).unwrap().server.max_connections_per_peer, 64);
            assert_eq!(
                parse(&["--max-connections-per-peer", "3"])
                    .unwrap()
                    .server
                    .max_connections_per_peer,
                3
            );
            assert!(parse(&["--max-connections-per-peer", "0"]).is_err());
        }

        #[test]
        fn metrics_listener_is_opt_in_and_loopback_only() {
            assert!(parse(&[]).unwrap().metrics_bind.is_none());
            assert!(parse(&["--metrics-bind", "0.0.0.0:9090"]).is_err());
            assert!(parse(&["--metrics-bind", "127.0.0.1:0"]).is_err());
            assert!(parse(&["--metrics-bind", "[::1]:9090"]).is_ok());
        }

        #[test]
        fn clock_reservation_is_bounded_opt_in_and_durable_only() {
            assert_eq!(parse(&[]).unwrap().clock_reservation_ms, 0);
            assert!(parse(&["--clock-reservation-ms", "250"]).is_err());
            assert!(parse(&[
                "--mode",
                "sync",
                "--journal",
                "cache.wal",
                "--clock-reservation-ms",
                "1001"
            ])
            .is_err());
            assert_eq!(
                parse(&[
                    "--mode",
                    "sync",
                    "--journal",
                    "cache.wal",
                    "--clock-reservation-ms",
                    "1000"
                ])
                .unwrap()
                .clock_reservation_ms,
                1000
            );
        }

        #[test]
        fn group_commit_is_opt_in_and_sync_only() {
            assert!(!parse(&[]).unwrap().group_commit);
            assert!(parse(&["--group-commit", "1"]).is_err());
            assert!(parse(&[
                "--mode",
                "buffered",
                "--journal",
                "cache.wal",
                "--group-commit",
                "1"
            ])
            .is_err());
            assert!(parse(&[
                "--mode",
                "sync",
                "--journal",
                "cache.wal",
                "--group-commit",
                "2"
            ])
            .is_err());
            assert!(
                parse(&[
                    "--mode",
                    "sync",
                    "--journal",
                    "cache.wal",
                    "--group-commit",
                    "1"
                ])
                .unwrap()
                .group_commit
            );
        }

        #[test]
        fn invalid_or_unsafe_configuration_is_rejected() {
            for args in [
                vec!["--bind", "0.0.0.0:6380"],
                vec!["--bind", "127.0.0.1:0"],
                vec!["--mode", "sync"],
                vec!["--journal", "/tmp/cache.wal"],
                vec!["--mode", "unexpected"],
                vec!["--max-entries", "0"],
                vec!["--max-entries", "-1"],
                vec!["--max-connections"],
                vec!["--unknown", "1"],
                vec!["--mode", "memory", "--mode", "sync"],
            ] {
                assert!(parse(&args).is_err(), "{args:?}");
            }
        }
    }
}
fn main() {
    #[cfg(unix)]
    if let Err(error) = unix::run() {
        eprintln!("nulang-fabric: {error}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        eprintln!("nulang-fabric standalone mode currently requires Unix");
        std::process::exit(1);
    }
}
