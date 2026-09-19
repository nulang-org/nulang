//! Cache-specific dispatch between RESP ingress and shard-local stores.
//!
//! Same-shard commands execute directly against CacheStore. Commands owned
//! by another local shard are copied once into a bounded queue. Commands owned
//! by another node are returned as explicit remote handoffs for the cluster
//! transport layer to carry. The local hot path never takes a global mutex and
//! never enters the actor mailbox scheduler.

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};

use super::cache::CacheStore;
use super::cache_cluster::{CacheEndpointMap, CacheRoutingMode};
use super::cache_routing::{CacheShardOwner, CacheSlotMap};
use super::resp::{parse_command, write_moved, RespParseError};
use super::resp_cache::{command_slot, execute_command, execute_frame, RespCommandSlot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDispatchConfigError {
    InvalidShardCount,
    InvalidQueueCapacity,
    InvalidLocalShard { shard: u16, shard_count: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDispatchError {
    Parse(RespParseError),
    UnknownSlot(u16),
    UnknownLocalShard(u16),
    QueueFull(u16),
    QueueDisconnected(u16),
    MissingEndpoint(CacheShardOwner),
}

impl From<RespParseError> for CacheDispatchError {
    fn from(value: RespParseError) -> Self {
        Self::Parse(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheReplyError {
    Empty,
    Disconnected,
    Parse(RespParseError),
}

pub struct CacheLocalReply {
    receiver: Receiver<Result<Vec<u8>, RespParseError>>,
}

impl CacheLocalReply {
    pub fn try_recv(&self) -> Result<Option<Vec<u8>>, CacheReplyError> {
        match self.receiver.try_recv() {
            Ok(Ok(bytes)) => Ok(Some(bytes)),
            Ok(Err(error)) => Err(CacheReplyError::Parse(error)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(CacheReplyError::Disconnected),
        }
    }

    pub fn recv(self) -> Result<Vec<u8>, CacheReplyError> {
        match self.receiver.recv() {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(error)) => Err(CacheReplyError::Parse(error)),
            Err(_) => Err(CacheReplyError::Disconnected),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRemoteRequest {
    pub slot: u16,
    pub owner: CacheShardOwner,
    pub placement_epoch: u64,
    pub frame: Vec<u8>,
    pub now_ms: u64,
}

pub enum CacheDispatchOutcome {
    Executed {
        consumed: usize,
    },
    LocalQueued {
        consumed: usize,
        shard: u16,
        reply: CacheLocalReply,
    },
    Remote {
        consumed: usize,
        request: CacheRemoteRequest,
    },
    Redirected {
        consumed: usize,
        slot: u16,
        owner: CacheShardOwner,
    },
}

struct CacheShardRequest {
    frame: Vec<u8>,
    now_ms: u64,
    reply: SyncSender<Result<Vec<u8>, RespParseError>>,
}

#[derive(Clone)]
pub struct CacheDispatchChannels {
    senders: Vec<SyncSender<CacheShardRequest>>,
}

impl CacheDispatchChannels {
    pub fn new(
        shard_count: u16,
        queue_capacity: usize,
    ) -> Result<(Self, Vec<CacheShardInbox>), CacheDispatchConfigError> {
        if shard_count == 0 {
            return Err(CacheDispatchConfigError::InvalidShardCount);
        }
        if queue_capacity == 0 {
            return Err(CacheDispatchConfigError::InvalidQueueCapacity);
        }

        let mut senders = Vec::with_capacity(shard_count as usize);
        let mut inboxes = Vec::with_capacity(shard_count as usize);
        for shard in 0..shard_count {
            let (sender, receiver) = mpsc::sync_channel(queue_capacity);
            senders.push(sender);
            inboxes.push(CacheShardInbox { shard, receiver });
        }

        Ok((Self { senders }, inboxes))
    }

    pub fn shard_count(&self) -> u16 {
        self.senders.len() as u16
    }

    fn try_send(&self, shard: u16, request: CacheShardRequest) -> Result<(), CacheDispatchError> {
        let Some(sender) = self.senders.get(shard as usize) else {
            return Err(CacheDispatchError::UnknownLocalShard(shard));
        };

        match sender.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(CacheDispatchError::QueueFull(shard)),
            Err(TrySendError::Disconnected(_)) => Err(CacheDispatchError::QueueDisconnected(shard)),
        }
    }
}

pub struct CacheShardInbox {
    shard: u16,
    receiver: Receiver<CacheShardRequest>,
}

impl CacheShardInbox {
    pub fn shard(&self) -> u16 {
        self.shard
    }

    pub fn try_process_one(&mut self, store: &mut CacheStore) -> bool {
        let request = match self.receiver.try_recv() {
            Ok(request) => request,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return false,
        };

        let mut out = Vec::with_capacity(64);
        let result = match execute_frame(store, &request.frame, request.now_ms, &mut out) {
            Ok(Some(consumed)) if consumed == request.frame.len() => Ok(out),
            Ok(Some(_)) | Ok(None) => Err(RespParseError::InvalidLength),
            Err(error) => Err(error),
        };
        let _ = request.reply.send(result);
        true
    }

    pub fn drain(&mut self, store: &mut CacheStore, max_commands: usize) -> usize {
        let mut processed = 0;
        while processed < max_commands && self.try_process_one(store) {
            processed += 1;
        }
        processed
    }
}

pub struct CacheDispatcher {
    local_node_id: u64,
    local_shard: u16,
    placement: CacheSlotMap,
    channels: CacheDispatchChannels,
    routing_mode: CacheRoutingMode,
    endpoints: CacheEndpointMap,
}

impl CacheDispatcher {
    pub fn new(
        local_node_id: u64,
        local_shard: u16,
        placement: CacheSlotMap,
        channels: CacheDispatchChannels,
    ) -> Result<Self, CacheDispatchConfigError> {
        if local_shard >= channels.shard_count() {
            return Err(CacheDispatchConfigError::InvalidLocalShard {
                shard: local_shard,
                shard_count: channels.shard_count(),
            });
        }

        Ok(Self {
            local_node_id,
            local_shard,
            placement,
            channels,
            routing_mode: CacheRoutingMode::Transparent,
            endpoints: CacheEndpointMap::new(),
        })
    }

    pub fn with_cluster_redirects(mut self, endpoints: CacheEndpointMap) -> Self {
        self.routing_mode = CacheRoutingMode::Redirect;
        self.endpoints = endpoints;
        self
    }

    pub fn routing_mode(&self) -> CacheRoutingMode {
        self.routing_mode
    }

    pub fn local_shard(&self) -> u16 {
        self.local_shard
    }

    pub fn placement(&self) -> &CacheSlotMap {
        &self.placement
    }

    pub fn install_placement(&mut self, placement: CacheSlotMap) {
        self.placement = placement;
    }

    pub fn dispatch_frame(
        &self,
        store: &mut CacheStore,
        input: &[u8],
        now_ms: u64,
        out: &mut Vec<u8>,
    ) -> Result<Option<CacheDispatchOutcome>, CacheDispatchError> {
        let Some((command, consumed)) = parse_command(input)? else {
            return Ok(None);
        };

        let slot = match command_slot(command) {
            RespCommandSlot::Unkeyed | RespCommandSlot::CrossSlot => {
                execute_command(store, command, now_ms, out);
                return Ok(Some(CacheDispatchOutcome::Executed { consumed }));
            }
            RespCommandSlot::Slot(slot) => slot,
        };

        let owner = self
            .placement
            .owner_for_slot(slot)
            .ok_or(CacheDispatchError::UnknownSlot(slot))?;

        let is_local_owner = owner.node_id == self.local_node_id && owner.shard == self.local_shard;

        if !is_local_owner && self.routing_mode == CacheRoutingMode::Redirect {
            let endpoint = self
                .endpoints
                .get(owner)
                .ok_or(CacheDispatchError::MissingEndpoint(owner))?;
            write_moved(out, slot, endpoint.target());
            return Ok(Some(CacheDispatchOutcome::Redirected {
                consumed,
                slot,
                owner,
            }));
        }

        if owner.node_id != self.local_node_id {
            return Ok(Some(CacheDispatchOutcome::Remote {
                consumed,
                request: CacheRemoteRequest {
                    slot,
                    owner,
                    placement_epoch: self.placement.epoch(),
                    frame: input[..consumed].to_vec(),
                    now_ms,
                },
            }));
        }

        if owner.shard == self.local_shard {
            execute_command(store, command, now_ms, out);
            return Ok(Some(CacheDispatchOutcome::Executed { consumed }));
        }

        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.channels.try_send(
            owner.shard,
            CacheShardRequest {
                frame: input[..consumed].to_vec(),
                now_ms,
                reply: reply_tx,
            },
        )?;

        Ok(Some(CacheDispatchOutcome::LocalQueued {
            consumed,
            shard: owner.shard,
            reply: CacheLocalReply { receiver: reply_rx },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::{redis_slot, CacheValueView};
    use super::super::cache_cluster::CacheAdvertisedEndpoint;
    use super::super::cache_routing::CacheSlotRange;
    use super::*;

    fn key_for_shard(map: &CacheSlotMap, shard: u16) -> Vec<u8> {
        for i in 0..10_000 {
            let key = format!("key-{i}").into_bytes();
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
    fn same_shard_command_executes_without_queueing() {
        let map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&map, 0);
        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, map, channels).unwrap();
        let command = frame(&[b"SET", &key, b"value"]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        let outcome = dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        assert!(matches!(outcome, CacheDispatchOutcome::Executed { .. }));
        assert_eq!(out, b"+OK\r\n");
        assert_eq!(store.get(&key, 0), Some(CacheValueView::Bytes(b"value")));
    }

    #[test]
    fn other_local_shard_uses_bounded_request_reply_queue() {
        let map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&map, 1);
        let (channels, mut inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, map, channels).unwrap();
        let command = frame(&[b"SET", &key, b"value"]);
        let mut ingress_store = CacheStore::new();
        let mut owner_store = CacheStore::new();
        let mut out = Vec::new();

        let outcome = dispatcher
            .dispatch_frame(&mut ingress_store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        let CacheDispatchOutcome::LocalQueued { shard, reply, .. } = outcome else {
            panic!("expected local queued dispatch");
        };
        assert_eq!(shard, 1);
        assert!(out.is_empty());
        assert!(ingress_store.is_empty());
        assert!(inboxes[1].try_process_one(&mut owner_store));
        assert_eq!(reply.recv().unwrap(), b"+OK\r\n");
        assert_eq!(
            owner_store.get(&key, 0),
            Some(CacheValueView::Bytes(b"value"))
        );
    }

    #[test]
    fn local_queue_backpressure_is_explicit() {
        let map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&map, 1);
        let (channels, _inboxes) = CacheDispatchChannels::new(2, 1).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, map, channels).unwrap();
        let command = frame(&[b"GET", &key]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        let first = dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();
        assert!(matches!(first, CacheDispatchOutcome::LocalQueued { .. }));

        assert!(matches!(
            dispatcher.dispatch_frame(&mut store, &command, 0, &mut out),
            Err(CacheDispatchError::QueueFull(1))
        ));
    }

    #[test]
    fn remote_owner_returns_transport_handoff_without_local_mutation() {
        let mut map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = b"remote-key";
        let slot = redis_slot(key);
        map.apply_epoch(
            1,
            &[CacheSlotRange {
                start: slot,
                end: slot,
                owner: CacheShardOwner {
                    node_id: 9,
                    shard: 3,
                },
            }],
        )
        .unwrap();

        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, map, channels).unwrap();
        let command = frame(&[b"SET", key, b"value"]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        let outcome = dispatcher
            .dispatch_frame(&mut store, &command, 55, &mut out)
            .unwrap()
            .unwrap();

        let CacheDispatchOutcome::Remote { request, .. } = outcome else {
            panic!("expected remote handoff");
        };
        assert_eq!(request.slot, slot);
        assert_eq!(request.owner.node_id, 9);
        assert_eq!(request.owner.shard, 3);
        assert_eq!(request.placement_epoch, 1);
        assert_eq!(request.frame, command);
        assert_eq!(request.now_ms, 55);
        assert!(store.is_empty());
        assert!(out.is_empty());
    }

    #[test]
    fn cross_slot_command_fails_before_queueing() {
        let map = CacheSlotMap::new_local(1, 2).unwrap();
        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, map, channels).unwrap();
        let command = frame(&[b"MGET", b"a", b"b"]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        let outcome = dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        assert!(matches!(outcome, CacheDispatchOutcome::Executed { .. }));
        assert_eq!(
            out,
            b"-CROSSSLOT Keys in request don't hash to the same slot\r\n"
        );
    }

    #[test]
    fn redirect_mode_returns_moved_for_other_local_shard_without_queueing() {
        let map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&map, 1);
        let owner = map.owner_for_key(&key);
        let (channels, mut inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(owner, CacheAdvertisedEndpoint::new("127.0.0.1", 7001));
        let dispatcher = CacheDispatcher::new(1, 0, map, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);
        let command = frame(&[b"GET", &key]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        let outcome = dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        let CacheDispatchOutcome::Redirected {
            slot,
            owner: redirected_owner,
            ..
        } = outcome
        else {
            panic!("expected MOVED redirect");
        };
        assert_eq!(slot, redis_slot(&key));
        assert_eq!(redirected_owner, owner);

        let expected = format!("-MOVED {} 127.0.0.1:7001\r\n", slot);
        assert_eq!(out, expected.as_bytes());

        let mut owner_store = CacheStore::new();
        assert!(!inboxes[1].try_process_one(&mut owner_store));
    }

    #[test]
    fn redirect_mode_returns_moved_for_remote_node() {
        let mut map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = b"remote-redirect";
        let slot = redis_slot(key);
        let owner = CacheShardOwner {
            node_id: 9,
            shard: 3,
        };
        map.apply_epoch(
            1,
            &[CacheSlotRange {
                start: slot,
                end: slot,
                owner,
            }],
        )
        .unwrap();

        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(owner, CacheAdvertisedEndpoint::new("cache-nine", 7003));
        let dispatcher = CacheDispatcher::new(1, 0, map, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);
        let command = frame(&[b"SET", key, b"value"]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        let outcome = dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        assert!(matches!(
            outcome,
            CacheDispatchOutcome::Redirected {
                slot: redirected_slot,
                owner: redirected_owner,
                ..
            } if redirected_slot == slot && redirected_owner == owner
        ));
        let expected = format!("-MOVED {} cache-nine:7003\r\n", slot);
        assert_eq!(out, expected.as_bytes());
        assert!(store.is_empty());
    }

    #[test]
    fn redirect_mode_fails_closed_when_owner_has_no_endpoint() {
        let map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&map, 1);
        let owner = map.owner_for_key(&key);
        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, map, channels)
            .unwrap()
            .with_cluster_redirects(CacheEndpointMap::new());
        let command = frame(&[b"GET", &key]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        assert!(matches!(
            dispatcher.dispatch_frame(&mut store, &command, 0, &mut out),
            Err(CacheDispatchError::MissingEndpoint(missing)) if missing == owner
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn keyless_ping_executes_directly() {
        let map = CacheSlotMap::new_local(1, 2).unwrap();
        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, map, channels).unwrap();
        let command = frame(&[b"PING"]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        let outcome = dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        assert!(matches!(outcome, CacheDispatchOutcome::Executed { .. }));
        assert_eq!(out, b"+PONG\r\n");
    }
}
