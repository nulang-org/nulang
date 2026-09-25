use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::runtime::{
    recover_cache, CacheAdvertisedEndpoint, CacheConfig, CacheDispatchChannels, CacheDispatcher,
    CacheDurabilityMode, CacheEndpointMap, CacheEvictionPolicy, CacheServerClock,
    CacheServerConfig, CacheShardServer, CacheSlotMap, CacheStore, CacheWal, DurableCacheStore,
};

struct Args {
    bind: SocketAddr,
    durability: CacheDurabilityMode,
    data_dir: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = env::args().skip(1);
    let mut bind = "127.0.0.1:6380".to_string();
    let mut durability = CacheDurabilityMode::Memory;
    let mut data_dir = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                bind = args
                    .next()
                    .ok_or_else(|| "--bind requires HOST:PORT".to_string())?;
            }
            "--durability" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--durability requires memory|buffered|synced".to_string())?;
                durability = match value.as_str() {
                    "memory" => CacheDurabilityMode::Memory,
                    "buffered" => CacheDurabilityMode::BufferedJournal,
                    "synced" => CacheDurabilityMode::SyncedJournal,
                    _ => {
                        return Err(format!(
                            "invalid --durability {value:?}; expected memory|buffered|synced"
                        ));
                    }
                };
            }
            "--data-dir" => {
                data_dir = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--data-dir requires a path".to_string())?,
                ));
            }
            "-h" | "--help" => {
                println!(
                    "usage: nulang-cache [--bind HOST:PORT] \
                     [--durability memory|buffered|synced] [--data-dir PATH]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if durability != CacheDurabilityMode::Memory && data_dir.is_none() {
        return Err("--data-dir is required for buffered or synced durability".to_string());
    }

    let bind = bind
        .parse()
        .map_err(|error| format!("invalid --bind address {bind:?}: {error}"))?;
    Ok(Args {
        bind,
        durability,
        data_dir,
    })
}

fn unix_now_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let placement = CacheSlotMap::new_local(1, 1)
        .map_err(|error| format!("failed to create cache placement: {error:?}"))?;
    let owner = placement
        .owner_for_slot(0)
        .ok_or_else(|| "single-shard placement has no slot owner".to_string())?;

    let (channels, mut inboxes) = CacheDispatchChannels::new(1, 1_024)
        .map_err(|error| format!("failed to create cache dispatch channels: {error:?}"))?;

    let mut endpoints = CacheEndpointMap::new();
    endpoints.insert(
        owner,
        CacheAdvertisedEndpoint::new(&args.bind.ip().to_string(), args.bind.port()),
    );

    let dispatcher = CacheDispatcher::new(1, 0, placement, channels)
        .map_err(|error| format!("failed to create cache dispatcher: {error:?}"))?
        .with_cluster_redirects(endpoints);
    let inbox = inboxes
        .pop()
        .ok_or_else(|| "single-shard cache inbox was not created".to_string())?;
    let clock = CacheServerClock::new();
    let config = CacheServerConfig::default();

    let mut server = match args.durability {
        CacheDurabilityMode::Memory => CacheShardServer::bind(
            args.bind,
            dispatcher,
            inbox,
            CacheStore::new(),
            config,
            clock,
        ),
        CacheDurabilityMode::BufferedJournal | CacheDurabilityMode::SyncedJournal => {
            let data_dir = args
                .data_dir
                .as_ref()
                .expect("validated journaled data directory");
            std::fs::create_dir_all(data_dir)
                .map_err(|error| format!("failed to create cache data directory: {error}"))?;

            let snapshot_path = data_dir.join("cache.snapshot");
            let wal_path = data_dir.join("cache.wal");
            let store_now_ms = clock.now_ms();
            let wall_now_ms = unix_now_ms()?;

            let (store, report) = recover_cache(
                &snapshot_path,
                &wal_path,
                CacheConfig::default(),
                CacheEvictionPolicy::S3Fifo,
                store_now_ms,
                wall_now_ms,
            )
            .map_err(|error| format!("cache recovery failed: {error}"))?;

            // A snapshot may legitimately be newer than an old or absent WAL.
            // Before new writes, advance the WAL base to the snapshot sequence
            // so future records are never assigned sequence numbers recovery
            // would treat as already included in the snapshot.
            let wal =
                if !wal_path.exists() || report.wal_last_sequence < report.snapshot_sequence {
                    CacheWal::create_after(&wal_path, report.snapshot_sequence)
                } else {
                    CacheWal::open(&wal_path)
                }
                .map_err(|error| format!("failed to open cache WAL: {error}"))?;

            eprintln!(
                "nulang-cache recovered snapshot_seq={} wal_base={} wal_last={} replayed={}",
                report.snapshot_sequence,
                report.wal_base_sequence,
                report.wal_last_sequence,
                report.replayed_records
            );

            let durable = DurableCacheStore::with_wal(store, wal, args.durability)
                .map_err(|error| format!("failed to configure cache durability: {error}"))?;
            CacheShardServer::bind_durable(args.bind, dispatcher, inbox, durable, config, clock)
        }
    }
    .map_err(|error| format!("failed to bind cache server: {error:?}"))?;

    eprintln!(
        "nulang-cache listening on {} durability={:?}",
        server
            .local_addr()
            .map_err(|error| format!("failed to read bound address: {error}"))?,
        server.durability_mode()
    );

    server
        .run()
        .map_err(|error| format!("cache server failed: {error:?}"))
}
