use std::env;
use std::net::SocketAddr;

use nulang::runtime::{
    CacheAdvertisedEndpoint, CacheDispatchChannels, CacheDispatcher, CacheEndpointMap,
    CacheServerClock, CacheServerConfig, CacheShardServer, CacheSlotMap, CacheStore,
};

fn parse_bind_addr() -> Result<SocketAddr, String> {
    let mut args = env::args().skip(1);
    let mut bind = "127.0.0.1:6380".to_string();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                bind = args
                    .next()
                    .ok_or_else(|| "--bind requires HOST:PORT".to_string())?;
            }
            "-h" | "--help" => {
                println!("usage: nulang-cache [--bind HOST:PORT]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    bind.parse()
        .map_err(|error| format!("invalid --bind address {bind:?}: {error}"))
}

fn main() -> Result<(), String> {
    let bind_addr = parse_bind_addr()?;
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
        CacheAdvertisedEndpoint::new(&bind_addr.ip().to_string(), bind_addr.port()),
    );

    let dispatcher = CacheDispatcher::new(1, 0, placement, channels)
        .map_err(|error| format!("failed to create cache dispatcher: {error:?}"))?
        .with_cluster_redirects(endpoints);
    let inbox = inboxes
        .pop()
        .ok_or_else(|| "single-shard cache inbox was not created".to_string())?;

    let mut server = CacheShardServer::bind(
        bind_addr,
        dispatcher,
        inbox,
        CacheStore::new(),
        CacheServerConfig::default(),
        CacheServerClock::new(),
    )
    .map_err(|error| format!("failed to bind cache server: {error:?}"))?;

    eprintln!(
        "nulang-cache listening on {}",
        server
            .local_addr()
            .map_err(|error| format!("failed to read bound address: {error}"))?
    );

    server
        .run()
        .map_err(|error| format!("cache server failed: {error:?}"))
}
