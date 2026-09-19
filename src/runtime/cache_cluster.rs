//! Redis-Cluster-facing endpoint metadata for the cache tier.

use std::collections::HashMap;

use super::cache_routing::CacheShardOwner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRoutingMode {
    Transparent,
    Redirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheAdvertisedEndpoint {
    target: Vec<u8>,
}

impl CacheAdvertisedEndpoint {
    pub fn new(host: &str, port: u16) -> Self {
        let mut target = Vec::with_capacity(host.len() + 6);
        target.extend_from_slice(host.as_bytes());
        target.push(b':');
        write_u16_decimal(&mut target, port);
        Self { target }
    }

    /// Redis MOVED target in host:port form. An empty host is valid and
    /// produces ":port", which tells a client to reuse the current host.
    pub fn target(&self) -> &[u8] {
        &self.target
    }
}

#[derive(Debug, Clone, Default)]
pub struct CacheEndpointMap {
    endpoints: HashMap<CacheShardOwner, CacheAdvertisedEndpoint>,
}

impl CacheEndpointMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        owner: CacheShardOwner,
        endpoint: CacheAdvertisedEndpoint,
    ) -> Option<CacheAdvertisedEndpoint> {
        self.endpoints.insert(owner, endpoint)
    }

    pub fn get(&self, owner: CacheShardOwner) -> Option<&CacheAdvertisedEndpoint> {
        self.endpoints.get(&owner)
    }

    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }
}

fn write_u16_decimal(out: &mut Vec<u8>, mut value: u16) {
    let mut buf = [0u8; 5];
    let mut cursor = buf.len();

    if value == 0 {
        out.push(b'0');
        return;
    }

    while value != 0 {
        cursor -= 1;
        buf[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    out.extend_from_slice(&buf[cursor..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_preformats_redis_target() {
        let endpoint = CacheAdvertisedEndpoint::new("127.0.0.1", 6381);
        assert_eq!(endpoint.target(), b"127.0.0.1:6381");

        let same_host = CacheAdvertisedEndpoint::new("", 7002);
        assert_eq!(same_host.target(), b":7002");
    }

    #[test]
    fn endpoint_map_is_keyed_by_physical_owner() {
        let owner = CacheShardOwner {
            node_id: 7,
            shard: 3,
        };
        let mut endpoints = CacheEndpointMap::new();
        assert!(endpoints
            .insert(owner, CacheAdvertisedEndpoint::new("cache-3", 7003))
            .is_none());
        assert_eq!(endpoints.get(owner).unwrap().target(), b"cache-3:7003");
    }
}
