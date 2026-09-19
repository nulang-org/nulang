//! Multi-shard lifecycle wrapper for the RESP cache service.
//!
//! This module binds every physical shard endpoint before constructing the
//! dispatchers so Redis Cluster discovery and MOVED responses advertise the
//! exact listening ports, including when the caller requests ephemeral ports.

#![cfg(feature = "cache-server")]

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener as StdTcpListener};
use std::thread::{self, JoinHandle};

use super::cache::CacheStore;
use super::cache_cluster::{CacheAdvertisedEndpoint, CacheEndpointMap};
use super::cache_dispatch::{CacheDispatchChannels, CacheDispatchConfigError, CacheDispatcher};
use super::cache_routing::{CachePlacementError, CacheShardOwner, CacheSlotMap};
use super::cache_server::{
    CacheServerClock, CacheServerConfig, CacheServerError, CacheShardServer,
    CacheShardServerControl,
};
use super::pin_current_thread_to_cpu;

#[derive(Debug)]
pub enum CacheServiceError {
    Io(io::Error),
    Placement(CachePlacementError),
    Dispatch(CacheDispatchConfigError),
    Server(CacheServerError),
    InvalidConfig(&'static str),
    ShardPanicked(u16),
    ShardFailed { shard: u16, error: CacheServerError },
}

impl From<io::Error> for CacheServiceError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<CachePlacementError> for CacheServiceError {
    fn from(value: CachePlacementError) -> Self {
        Self::Placement(value)
    }
}

impl From<CacheDispatchConfigError> for CacheServiceError {
    fn from(value: CacheDispatchConfigError) -> Self {
        Self::Dispatch(value)
    }
}

impl From<CacheServerError> for CacheServiceError {
    fn from(value: CacheServerError) -> Self {
        Self::Server(value)
    }
}

pub struct CacheService {
    servers: Vec<CacheShardServer>,
    addresses: Vec<SocketAddr>,
    placement: CacheSlotMap,
    endpoints: CacheEndpointMap,
}

impl CacheService {
    /// Bind a complete local cache service.
    ///
    /// If base_port is zero, the OS chooses a distinct ephemeral port for each
    /// shard. Otherwise shard i binds base_port + i.
    pub fn bind_local(
        node_id: u64,
        bind_ip: IpAddr,
        base_port: u16,
        shard_count: u16,
        advertised_host: &str,
        queue_capacity: usize,
        server_config: CacheServerConfig,
    ) -> Result<Self, CacheServiceError> {
        if shard_count == 0 {
            return Err(CacheServiceError::InvalidConfig(
                "shard_count must be non-zero",
            ));
        }
        if queue_capacity == 0 {
            return Err(CacheServiceError::InvalidConfig(
                "queue_capacity must be non-zero",
            ));
        }
        if advertised_host.is_empty() {
            return Err(CacheServiceError::InvalidConfig(
                "advertised_host must be non-empty",
            ));
        }

        let placement = CacheSlotMap::new_local(node_id, shard_count)?;
        let (channels, inboxes) = CacheDispatchChannels::new(shard_count, queue_capacity)?;
        let clock = CacheServerClock::new();

        let mut listeners = Vec::with_capacity(shard_count as usize);
        let mut addresses = Vec::with_capacity(shard_count as usize);

        for shard in 0..shard_count {
            let port = if base_port == 0 {
                0
            } else {
                base_port.checked_add(shard).ok_or(
                    CacheServiceError::InvalidConfig("base_port + shard exceeds u16"),
                )?
            };
            let listener = StdTcpListener::bind(SocketAddr::new(bind_ip, port))?;
            listener.set_nonblocking(true)?;
            addresses.push(listener.local_addr()?);
            listeners.push(listener);
        }

        let mut endpoints = CacheEndpointMap::new();
        for (shard, address) in addresses.iter().copied().enumerate() {
            endpoints.insert(
                CacheShardOwner {
                    node_id,
                    shard: shard as u16,
                },
                CacheAdvertisedEndpoint::new(advertised_host, address.port()),
            );
        }

        let mut servers = Vec::with_capacity(shard_count as usize);
        for (shard, (listener, inbox)) in listeners
            .into_iter()
            .zip(inboxes.into_iter())
            .enumerate()
        {
            let dispatcher = CacheDispatcher::new(
                node_id,
                shard as u16,
                placement.clone(),
                channels.clone(),
            )?
            .with_cluster_redirects(endpoints.clone());

            servers.push(CacheShardServer::from_std_listener(
                listener,
                dispatcher,
                inbox,
                CacheStore::new(),
                server_config.clone(),
                clock.clone(),
            )?);
        }

        Ok(Self {
            servers,
            addresses,
            placement,
            endpoints,
        })
    }

    pub fn shard_count(&self) -> usize {
        self.servers.len()
    }

    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    pub fn placement(&self) -> &CacheSlotMap {
        &self.placement
    }

    pub fn endpoints(&self) -> &CacheEndpointMap {
        &self.endpoints
    }

    /// Spawn one OS thread per physical cache shard.
    ///
    /// When pin_cores is true, shard i attempts to pin itself to logical CPU i
    /// using the same runtime primitive as the actor-shard launcher.
    pub fn spawn(self, pin_cores: bool) -> Result<CacheServiceHandle, CacheServiceError> {
        let CacheService {
            servers,
            addresses,
            placement,
            endpoints,
        } = self;

        let mut running = Vec::<RunningShard>::with_capacity(servers.len());

        for mut server in servers {
            let shard = server.shard();
            let control = server.control();
            let name = format!("nulang-cache-{shard}");

            let join = match thread::Builder::new().name(name).spawn(move || {
                if pin_cores {
                    let _ = pin_current_thread_to_cpu(shard as usize);
                }
                server.run()
            }) {
                Ok(join) => join,
                Err(error) => {
                    for shard in &running {
                        shard.control.shutdown();
                    }
                    for mut shard in running {
                        if let Some(join) = shard.join.take() {
                            let _ = join.join();
                        }
                    }
                    return Err(CacheServiceError::Io(error));
                }
            };

            running.push(RunningShard {
                shard,
                control,
                join: Some(join),
            });
        }

        Ok(CacheServiceHandle {
            shards: running,
            addresses,
            placement,
            endpoints,
        })
    }
}

struct RunningShard {
    shard: u16,
    control: CacheShardServerControl,
    join: Option<JoinHandle<Result<(), CacheServerError>>>,
}

pub struct CacheServiceHandle {
    shards: Vec<RunningShard>,
    addresses: Vec<SocketAddr>,
    placement: CacheSlotMap,
    endpoints: CacheEndpointMap,
}

impl CacheServiceHandle {
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    pub fn placement(&self) -> &CacheSlotMap {
        &self.placement
    }

    pub fn endpoints(&self) -> &CacheEndpointMap {
        &self.endpoints
    }

    pub fn shutdown(&self) {
        for shard in &self.shards {
            shard.control.shutdown();
        }
    }

    pub fn shutdown_and_join(mut self) -> Result<(), CacheServiceError> {
        self.shutdown();

        let mut first_error = None;
        for shard in &mut self.shards {
            let Some(join) = shard.join.take() else {
                continue;
            };
            match join.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(CacheServiceError::ShardFailed {
                            shard: shard.shard,
                            error,
                        });
                    }
                }
                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some(CacheServiceError::ShardPanicked(shard.shard));
                    }
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for CacheServiceHandle {
    fn drop(&mut self) {
        for shard in &self.shards {
            shard.control.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::cache::redis_slot;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpStream};
    use std::time::Duration;

    fn bind_two_shards() -> CacheService {
        CacheService::bind_local(
            42,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            2,
            "127.0.0.1",
            64,
            CacheServerConfig::default(),
        )
        .unwrap()
    }

    fn key_for_shard(map: &CacheSlotMap, shard: u16) -> Vec<u8> {
        for i in 0..100_000 {
            let key = format!("service-key-{i}").into_bytes();
            if map.owner_for_key(&key).shard == shard {
                return key;
            }
        }
        panic!("could not find key for shard");
    }

    fn frame(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = format!("*{}\r\n", parts.len()).into_bytes();
        for part in parts {
            out.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    #[test]
    fn bind_local_reserves_unique_advertised_shard_endpoints() {
        let service = bind_two_shards();
        assert_eq!(service.shard_count(), 2);
        assert_ne!(service.addresses()[0].port(), 0);
        assert_ne!(service.addresses()[1].port(), 0);
        assert_ne!(service.addresses()[0].port(), service.addresses()[1].port());
        assert_eq!(service.placement().slot_ranges().len(), 2);

        for shard in 0..2u16 {
            let owner = CacheShardOwner {
                node_id: 42,
                shard,
            };
            let endpoint = service.endpoints().get(owner).unwrap();
            assert_eq!(endpoint.host(), b"127.0.0.1");
            assert_eq!(endpoint.port(), service.addresses()[shard as usize].port());
        }
    }

    #[test]
    fn spawned_service_serves_owner_and_redirects_wrong_shard() {
        let service = bind_two_shards();
        let addresses = service.addresses().to_vec();
        let placement = service.placement().clone();
        let key = key_for_shard(&placement, 1);
        let slot = redis_slot(&key);
        let handle = service.spawn(false).unwrap();

        let mut wrong = TcpStream::connect(addresses[0]).unwrap();
        wrong
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        wrong.write_all(&frame(&[b"GET", &key])).unwrap();

        let expected_redirect = format!(
            "-MOVED {} 127.0.0.1:{}\r\n",
            slot, addresses[1].port()
        );
        let mut redirect = vec![0u8; expected_redirect.len()];
        wrong.read_exact(&mut redirect).unwrap();
        assert_eq!(redirect, expected_redirect.as_bytes());

        let mut owner = TcpStream::connect(addresses[1]).unwrap();
        owner
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let request = [frame(&[b"SET", &key, b"value"]), frame(&[b"GET", &key])].concat();
        owner.write_all(&request).unwrap();

        let mut response = [0u8; 16];
        owner.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"+OK\r\n$5\r\nvalue\r\n");

        handle.shutdown_and_join().unwrap();
    }
}
