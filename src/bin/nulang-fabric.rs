#[cfg(unix)]
mod unix {
    use nulang::runtime::{
        cache::{CacheLimits, CacheStore},
        cache_cluster::{CacheAdvertisedEndpoint, CacheEndpointMap},
        cache_dispatch::{CacheDispatchChannels, CacheDispatcher},
        cache_routing::CacheSlotMap,
        cache_server::{
            AutomaticCheckpointConfig, CacheDurableConfig, CacheMutualTls, CacheServerClock,
            CacheServerConfig, CacheShardServer,
        },
        cache_wal::{WalLimits, WalSync},
    };
    use std::{collections::HashSet, io::Write, net::SocketAddr, path::PathBuf};

    const HELP: &str =
        "nulang-fabric [--bind 127.0.0.1:6380] [--mode memory|buffered|sync] [--journal PATH]
    --max-entries N --max-arena-bytes N --max-connections N
    --max-connection-bytes N --max-input-bytes N --max-output-bytes N --max-pipeline-depth N
    --max-record-bytes N --max-journal-bytes N
    --checkpoint-percent 0..99 (default 70; 0 disables)
    --checkpoint-min-savings-percent 1..99 (default 10)
    --checkpoint-retry-ms N (default 5000)
    --shutdown-drain-ms 0..60000 (default 5000)
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
            if config.bind.port() == 0 || (!config.bind.ip().is_loopback() && tls_count != 3) {
                return Err("non-loopback bind requires mutual TLS and a nonzero port".into());
            }
            if (config.mode == Mode::Memory) != config.journal.is_none() {
                return Err("a journal path is required exactly in buffered or sync mode".into());
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
            if self.mode != Mode::Memory && self.checkpoint.trigger_percent != 0 {
                server
                    .enable_automatic_checkpoint(self.checkpoint)
                    .map_err(|e| format!("{e:?}"))?;
            }
            Ok(server)
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
        let signals = block_termination_signals()?;
        let mut server = config.bind()?;
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
        server
            .run_with_shutdown_timeout(timeout)
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
