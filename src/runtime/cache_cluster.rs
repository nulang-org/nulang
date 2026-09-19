//! Redis-Cluster-facing topology metadata and discovery commands.

use std::collections::HashMap;

use super::cache::redis_slot;
use super::cache_routing::{CacheShardOwner, CacheSlotMap};
use super::resp::{write_array_len, write_bulk, write_error, write_integer, RespCommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRoutingMode {
    Transparent,
    Redirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheAdvertisedEndpoint {
    host: Vec<u8>,
    port: u16,
    target: Vec<u8>,
}

impl CacheAdvertisedEndpoint {
    pub fn new(host: &str, port: u16) -> Self {
        let host = host.as_bytes().to_vec();
        let mut target = Vec::with_capacity(host.len() + 6);
        target.extend_from_slice(&host);
        target.push(b':');
        write_u16_decimal(&mut target, port);
        Self { host, port, target }
    }

    pub fn host(&self) -> &[u8] {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheClusterCommandError {
    MissingEndpoint(CacheShardOwner),
}

/// Execute the read-only Redis Cluster commands needed by cluster-aware
/// clients. Returns None when the command is not CLUSTER.
pub fn execute_cluster_command(
    command: RespCommand<'_>,
    placement: &CacheSlotMap,
    endpoints: &CacheEndpointMap,
    out: &mut Vec<u8>,
) -> Option<Result<(), CacheClusterCommandError>> {
    if !command.name().eq_ignore_ascii_case(b"CLUSTER") {
        return None;
    }

    if command.argc() == 0 {
        write_error(out, b"ERR wrong number of arguments for 'cluster' command");
        return Some(Ok(()));
    }

    let mut args = command.args();
    let subcommand = args.next().expect("validated CLUSTER subcommand");

    if subcommand.eq_ignore_ascii_case(b"KEYSLOT") {
        if command.argc() != 2 {
            write_error(out, b"ERR wrong number of arguments for 'cluster|keyslot' command");
            return Some(Ok(()));
        }
        let key = args.next().expect("validated KEYSLOT key");
        write_integer(out, redis_slot(key) as i64);
        return Some(Ok(()));
    }

    if subcommand.eq_ignore_ascii_case(b"SLOTS") {
        if command.argc() != 1 {
            write_error(out, b"ERR wrong number of arguments for 'cluster|slots' command");
            return Some(Ok(()));
        }
        return Some(write_cluster_slots(placement, endpoints, out));
    }

    if subcommand.eq_ignore_ascii_case(b"SHARDS") {
        if command.argc() != 1 {
            write_error(out, b"ERR wrong number of arguments for 'cluster|shards' command");
            return Some(Ok(()));
        }
        return Some(write_cluster_shards(placement, endpoints, out));
    }

    write_error(out, b"ERR unknown CLUSTER subcommand");
    Some(Ok(()))
}

fn write_cluster_slots(
    placement: &CacheSlotMap,
    endpoints: &CacheEndpointMap,
    out: &mut Vec<u8>,
) -> Result<(), CacheClusterCommandError> {
    let ranges = placement.slot_ranges();

    for range in &ranges {
        if endpoints.get(range.owner).is_none() {
            return Err(CacheClusterCommandError::MissingEndpoint(range.owner));
        }
    }

    write_array_len(out, ranges.len());
    for range in ranges {
        let endpoint = endpoints
            .get(range.owner)
            .expect("topology endpoint prevalidated");

        write_array_len(out, 3);
        write_integer(out, range.start as i64);
        write_integer(out, range.end as i64);

        write_array_len(out, 3);
        write_bulk(out, endpoint.host());
        write_integer(out, endpoint.port() as i64);
        let node_id = redis_node_id(range.owner);
        write_bulk(out, &node_id);
    }

    Ok(())
}

fn write_cluster_shards(
    placement: &CacheSlotMap,
    endpoints: &CacheEndpointMap,
    out: &mut Vec<u8>,
) -> Result<(), CacheClusterCommandError> {
    let mut shards: Vec<(CacheShardOwner, Vec<(u16, u16)>)> = Vec::new();

    for range in placement.slot_ranges() {
        if let Some((_, ranges)) = shards.iter_mut().find(|(owner, _)| *owner == range.owner) {
            ranges.push((range.start, range.end));
        } else {
            shards.push((range.owner, vec![(range.start, range.end)]));
        }
    }

    for (owner, _) in &shards {
        if endpoints.get(*owner).is_none() {
            return Err(CacheClusterCommandError::MissingEndpoint(*owner));
        }
    }

    write_array_len(out, shards.len());
    for (owner, ranges) in shards {
        let endpoint = endpoints.get(owner).expect("topology endpoint prevalidated");

        // RESP2 map representation: ["slots", [...], "nodes", [...]]
        write_array_len(out, 4);
        write_bulk(out, b"slots");
        write_array_len(out, ranges.len() * 2);
        for (start, end) in ranges {
            write_integer(out, start as i64);
            write_integer(out, end as i64);
        }

        write_bulk(out, b"nodes");
        write_array_len(out, 1);

        // RESP2 map representation with the stable fields Redis clients need.
        write_array_len(out, 14);
        write_bulk(out, b"id");
        let node_id = redis_node_id(owner);
        write_bulk(out, &node_id);
        write_bulk(out, b"port");
        write_integer(out, endpoint.port() as i64);
        write_bulk(out, b"ip");
        write_bulk(out, b"");
        write_bulk(out, b"endpoint");
        write_bulk(out, endpoint.host());
        write_bulk(out, b"role");
        write_bulk(out, b"master");
        write_bulk(out, b"replication-offset");
        write_integer(out, 0);
        write_bulk(out, b"health");
        write_bulk(out, b"online");
    }

    Ok(())
}

/// Stable 40-hex-character Redis node id derived from Nulang node/shard
/// identity. It remains stable across endpoint changes and slot movement.
pub fn redis_node_id(owner: CacheShardOwner) -> [u8; 40] {
    let mut raw = [0u8; 20];
    raw[..8].copy_from_slice(&owner.node_id.to_be_bytes());
    raw[8..10].copy_from_slice(&owner.shard.to_be_bytes());
    raw[10..18].copy_from_slice(&owner.node_id.rotate_left(23).to_be_bytes());
    raw[18..20].copy_from_slice(&owner.shard.rotate_left(7).to_be_bytes());

    let mut encoded = [0u8; 40];
    for (idx, byte) in raw.iter().copied().enumerate() {
        encoded[idx * 2] = hex_nibble(byte >> 4);
        encoded[idx * 2 + 1] = hex_nibble(byte & 0x0f);
    }
    encoded
}

fn hex_nibble(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'a' + (value - 10),
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
    use super::super::cache_routing::CacheSlotRange;
    use super::super::resp::parse_command;

    fn command(frame: &[u8]) -> RespCommand<'_> {
        parse_command(frame).unwrap().unwrap().0
    }

    #[test]
    fn endpoint_preformats_redis_target() {
        let endpoint = CacheAdvertisedEndpoint::new("127.0.0.1", 6381);
        assert_eq!(endpoint.host(), b"127.0.0.1");
        assert_eq!(endpoint.port(), 6381);
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

    #[test]
    fn redis_node_ids_are_stable_and_owner_specific() {
        let a = CacheShardOwner {
            node_id: 7,
            shard: 3,
        };
        let b = CacheShardOwner {
            node_id: 7,
            shard: 4,
        };
        assert_eq!(redis_node_id(a), redis_node_id(a));
        assert_ne!(redis_node_id(a), redis_node_id(b));
        assert!(redis_node_id(a).iter().all(u8::is_ascii_hexdigit));
    }

    #[test]
    fn cluster_keyslot_uses_the_same_router_hash() {
        let placement = CacheSlotMap::new_local(1, 1).unwrap();
        let endpoints = CacheEndpointMap::new();
        let mut out = Vec::new();

        execute_cluster_command(
            command(b"*3\r\n$7\r\nCLUSTER\r\n$7\r\nKEYSLOT\r\n$5\r\na{42}\r\n"),
            &placement,
            &endpoints,
            &mut out,
        )
        .unwrap()
        .unwrap();

        let expected = format!(":{}\r\n", redis_slot(b"a{42}"));
        assert_eq!(out, expected.as_bytes());
    }

    #[test]
    fn cluster_slots_emits_contiguous_ranges_and_master_endpoint() {
        let mut placement = CacheSlotMap::new_local(1, 1).unwrap();
        let owner_a = CacheShardOwner {
            node_id: 1,
            shard: 0,
        };
        let owner_b = CacheShardOwner {
            node_id: 2,
            shard: 0,
        };
        placement
            .apply_epoch(
                1,
                &[CacheSlotRange {
                    start: 100,
                    end: 199,
                    owner: owner_b,
                }],
            )
            .unwrap();

        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(owner_a, CacheAdvertisedEndpoint::new("a.local", 7000));
        endpoints.insert(owner_b, CacheAdvertisedEndpoint::new("b.local", 7001));

        let mut out = Vec::new();
        execute_cluster_command(
            command(b"*2\r\n$7\r\nCLUSTER\r\n$5\r\nSLOTS\r\n"),
            &placement,
            &endpoints,
            &mut out,
        )
        .unwrap()
        .unwrap();

        assert!(out.starts_with(b"*3\r\n"));
        assert!(out.windows(b"a.local".len()).any(|w| w == b"a.local"));
        assert!(out.windows(b"b.local".len()).any(|w| w == b"b.local"));
        assert!(out.windows(b":100\r\n".len()).any(|w| w == b":100\r\n"));
        assert!(out.windows(b":199\r\n".len()).any(|w| w == b":199\r\n"));
    }

    #[test]
    fn cluster_shards_groups_noncontiguous_ranges_by_owner() {
        let mut placement = CacheSlotMap::new_local(1, 1).unwrap();
        let owner_a = CacheShardOwner {
            node_id: 1,
            shard: 0,
        };
        let owner_b = CacheShardOwner {
            node_id: 2,
            shard: 0,
        };
        placement
            .apply_epoch(
                1,
                &[
                    CacheSlotRange {
                        start: 100,
                        end: 199,
                        owner: owner_b,
                    },
                    CacheSlotRange {
                        start: 300,
                        end: 399,
                        owner: owner_b,
                    },
                ],
            )
            .unwrap();

        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(owner_a, CacheAdvertisedEndpoint::new("a.local", 7000));
        endpoints.insert(owner_b, CacheAdvertisedEndpoint::new("b.local", 7001));

        let mut out = Vec::new();
        execute_cluster_command(
            command(b"*2\r\n$7\r\nCLUSTER\r\n$6\r\nSHARDS\r\n"),
            &placement,
            &endpoints,
            &mut out,
        )
        .unwrap()
        .unwrap();

        assert!(out.starts_with(b"*2\r\n"));
        assert!(out.windows(b"slots".len()).any(|w| w == b"slots"));
        assert!(out.windows(b"nodes".len()).any(|w| w == b"nodes"));
        assert!(out.windows(b"b.local".len()).any(|w| w == b"b.local"));
        assert!(out.windows(b":300\r\n".len()).any(|w| w == b":300\r\n"));
        assert!(out.windows(b":399\r\n".len()).any(|w| w == b":399\r\n"));
    }

    #[test]
    fn topology_fails_before_writing_partial_reply_when_endpoint_is_missing() {
        let placement = CacheSlotMap::new_local(1, 1).unwrap();
        let endpoints = CacheEndpointMap::new();
        let mut out = Vec::new();

        let result = execute_cluster_command(
            command(b"*2\r\n$7\r\nCLUSTER\r\n$5\r\nSLOTS\r\n"),
            &placement,
            &endpoints,
            &mut out,
        )
        .unwrap();

        assert!(matches!(
            result,
            Err(CacheClusterCommandError::MissingEndpoint(_))
        ));
        assert!(out.is_empty());
    }
}
