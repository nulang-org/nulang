//! Cache-specific dispatch between RESP ingress and shard-local stores.
//!
//! Same-shard commands execute directly against CacheStore. Commands owned
//! by another local shard are copied once into a bounded queue. Commands owned
//! by another node are returned as explicit remote handoffs for the cluster
//! transport layer to carry. The local hot path never takes a global mutex and
//! never enters the actor mailbox scheduler.

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, OnceLock};

use super::cache::CacheStore;
use super::cache_cluster::{
    execute_cluster_command, CacheClusterCommandError, CacheEndpointMap, CacheRoutingMode,
};
use super::cache_routing::{CacheShardOwner, CacheSlotMap};
use super::resp::{
    parse_command, write_ask, write_error, write_moved, write_simple, RespParseError,
};
use super::resp_cache::{
    command_key_presence, command_slot, execute_command, execute_frame, RespCommandKeyPresence,
    RespCommandSlot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDispatchConfigError {
    InvalidShardCount,
    InvalidQueueCapacity,
    InvalidLocalShard { shard: u16, shard_count: u16 },
    WakeAlreadyInstalled(u16),
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheDispatchContext {
    asking: bool,
}

impl CacheDispatchContext {
    pub fn asking(&self) -> bool {
        self.asking
    }
}

pub trait CacheDispatchWake: Send + Sync {
    fn wake(&self);
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
    reply_wake: Option<Arc<dyn CacheDispatchWake>>,
}

#[derive(Clone)]
pub struct CacheDispatchChannels {
    senders: Vec<SyncSender<CacheShardRequest>>,
    wakers: Arc<Vec<OnceLock<Arc<dyn CacheDispatchWake>>>>,
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

        let wakers = Arc::new(
            (0..shard_count)
                .map(|_| OnceLock::<Arc<dyn CacheDispatchWake>>::new())
                .collect(),
        );

        Ok((Self { senders, wakers }, inboxes))
    }

    pub fn shard_count(&self) -> u16 {
        self.senders.len() as u16
    }

    pub fn install_waker(
        &self,
        shard: u16,
        waker: Arc<dyn CacheDispatchWake>,
    ) -> Result<(), CacheDispatchConfigError> {
        let Some(slot) = self.wakers.get(shard as usize) else {
            return Err(CacheDispatchConfigError::InvalidLocalShard {
                shard,
                shard_count: self.shard_count(),
            });
        };
        slot.set(waker)
            .map_err(|_| CacheDispatchConfigError::WakeAlreadyInstalled(shard))
    }

    fn waker_for(&self, shard: u16) -> Option<Arc<dyn CacheDispatchWake>> {
        self.wakers
            .get(shard as usize)
            .and_then(OnceLock::get)
            .cloned()
    }

    fn try_send(&self, shard: u16, request: CacheShardRequest) -> Result<(), CacheDispatchError> {
        let Some(sender) = self.senders.get(shard as usize) else {
            return Err(CacheDispatchError::UnknownLocalShard(shard));
        };

        match sender.try_send(request) {
            Ok(()) => {
                if let Some(waker) = self.waker_for(shard) {
                    waker.wake();
                }
                Ok(())
            }
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
        let reply_sent = request.reply.send(result).is_ok();
        if reply_sent {
            if let Some(waker) = request.reply_wake {
                waker.wake();
            }
        }
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

    pub fn with_endpoints(mut self, endpoints: CacheEndpointMap) -> Self {
        self.endpoints = endpoints;
        self
    }

    pub fn with_cluster_redirects(mut self, endpoints: CacheEndpointMap) -> Self {
        self.routing_mode = CacheRoutingMode::Redirect;
        self.endpoints = endpoints;
        self
    }

    pub fn routing_mode(&self) -> CacheRoutingMode {
        self.routing_mode
    }

    pub fn local_node_id(&self) -> u64 {
        self.local_node_id
    }

    pub fn local_shard(&self) -> u16 {
        self.local_shard
    }

    pub fn shard_count(&self) -> u16 {
        self.channels.shard_count()
    }

    pub fn placement(&self) -> &CacheSlotMap {
        &self.placement
    }

    pub fn install_placement(&mut self, placement: CacheSlotMap) {
        self.placement = placement;
    }

    pub fn install_waker(
        &self,
        shard: u16,
        waker: Arc<dyn CacheDispatchWake>,
    ) -> Result<(), CacheDispatchConfigError> {
        self.channels.install_waker(shard, waker)
    }

    pub fn dispatch_frame(
        &self,
        store: &mut CacheStore,
        input: &[u8],
        now_ms: u64,
        out: &mut Vec<u8>,
    ) -> Result<Option<CacheDispatchOutcome>, CacheDispatchError> {
        let mut context = CacheDispatchContext::default();
        self.dispatch_frame_with_context(store, input, now_ms, &mut context, out)
    }

    pub fn dispatch_frame_with_context(
        &self,
        store: &mut CacheStore,
        input: &[u8],
        now_ms: u64,
        context: &mut CacheDispatchContext,
        out: &mut Vec<u8>,
    ) -> Result<Option<CacheDispatchOutcome>, CacheDispatchError> {
        let Some((command, consumed)) = parse_command(input)? else {
            return Ok(None);
        };

        if command.name().eq_ignore_ascii_case(b"ASKING") {
            context.asking = false;
            if command.argc() == 0 {
                context.asking = true;
                write_simple(out, b"OK");
            } else {
                write_error(out, b"ERR wrong number of arguments for 'asking' command");
            }
            return Ok(Some(CacheDispatchOutcome::Executed { consumed }));
        }

        // ASKING authorizes exactly one subsequent complete command.
        let asking = std::mem::take(&mut context.asking);

        if let Some(result) =
            execute_cluster_command(command, &self.placement, &self.endpoints, out)
        {
            match result {
                Ok(()) => return Ok(Some(CacheDispatchOutcome::Executed { consumed })),
                Err(CacheClusterCommandError::MissingEndpoint(owner)) => {
                    return Err(CacheDispatchError::MissingEndpoint(owner));
                }
            }
        }

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

        let local = CacheShardOwner {
            node_id: self.local_node_id,
            shard: self.local_shard,
        };
        let is_local_owner = owner == local;

        if self.routing_mode == CacheRoutingMode::Redirect {
            if let Some(migration) = self.placement.migration_for_slot(slot) {
                if local == migration.target && asking {
                    execute_command(store, command, now_ms, out);
                    return Ok(Some(CacheDispatchOutcome::Executed { consumed }));
                }

                if local == migration.source {
                    match command_key_presence(store, command, now_ms) {
                        RespCommandKeyPresence::AllPresent | RespCommandKeyPresence::NoKeys => {
                            execute_command(store, command, now_ms, out);
                            return Ok(Some(CacheDispatchOutcome::Executed { consumed }));
                        }
                        RespCommandKeyPresence::AllMissing => {
                            let endpoint = self
                                .endpoints
                                .get(migration.target)
                                .ok_or(CacheDispatchError::MissingEndpoint(migration.target))?;
                            write_ask(out, slot, endpoint.target());
                            return Ok(Some(CacheDispatchOutcome::Redirected {
                                consumed,
                                slot,
                                owner: migration.target,
                            }));
                        }
                        RespCommandKeyPresence::Mixed => {
                            write_error(
                                out,
                                b"TRYAGAIN Multiple keys request during rehashing of slot",
                            );
                            return Ok(Some(CacheDispatchOutcome::Executed { consumed }));
                        }
                    }
                }
            }
        }

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
                reply_wake: self.channels.waker_for(self.local_shard),
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
    fn migrating_source_asks_target_for_missing_key() {
        let mut map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&map, 0);
        let slot = redis_slot(&key);
        let source = map.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 1,
            shard: 1,
        };
        map.begin_migration(1, slot, source, target).unwrap();

        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(source, CacheAdvertisedEndpoint::new("127.0.0.1", 7000));
        endpoints.insert(target, CacheAdvertisedEndpoint::new("127.0.0.1", 7001));
        let dispatcher = CacheDispatcher::new(1, 0, map, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);
        let command = frame(&[b"GET", &key]);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        let expected = format!("-ASK {} 127.0.0.1:7001\r\n", slot);
        assert_eq!(out, expected.as_bytes());
    }

    #[test]
    fn migrating_source_serves_key_that_is_still_resident() {
        let mut map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&map, 0);
        let slot = redis_slot(&key);
        let source = map.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 1,
            shard: 1,
        };
        map.begin_migration(1, slot, source, target).unwrap();

        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(source, CacheAdvertisedEndpoint::new("127.0.0.1", 7000));
        endpoints.insert(target, CacheAdvertisedEndpoint::new("127.0.0.1", 7001));
        let dispatcher = CacheDispatcher::new(1, 0, map, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);
        let command = frame(&[b"GET", &key]);
        let mut store = CacheStore::new();
        store.set_bytes(&key, b"value", None, 0);
        let mut out = Vec::new();

        dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        assert_eq!(out, b"$5\r\nvalue\r\n");
    }

    #[test]
    fn importing_target_requires_one_shot_asking_context() {
        let mut map = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&map, 0);
        let slot = redis_slot(&key);
        let source = map.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 1,
            shard: 1,
        };
        map.begin_migration(1, slot, source, target).unwrap();

        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(source, CacheAdvertisedEndpoint::new("127.0.0.1", 7000));
        endpoints.insert(target, CacheAdvertisedEndpoint::new("127.0.0.1", 7001));
        let dispatcher = CacheDispatcher::new(1, 1, map, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);
        let asking = frame(&[b"ASKING"]);
        let set = frame(&[b"SET", &key, b"imported"]);
        let get = frame(&[b"GET", &key]);
        let mut store = CacheStore::new();
        let mut context = CacheDispatchContext::default();
        let mut out = Vec::new();

        dispatcher
            .dispatch_frame_with_context(&mut store, &set, 0, &mut context, &mut out)
            .unwrap()
            .unwrap();
        let moved = format!("-MOVED {} 127.0.0.1:7000\r\n", slot);
        assert_eq!(out, moved.as_bytes());

        out.clear();
        dispatcher
            .dispatch_frame_with_context(&mut store, &asking, 0, &mut context, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(out, b"+OK\r\n");
        assert!(context.asking());

        out.clear();
        dispatcher
            .dispatch_frame_with_context(&mut store, &set, 0, &mut context, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(out, b"+OK\r\n");
        assert!(!context.asking());

        out.clear();
        dispatcher
            .dispatch_frame_with_context(&mut store, &get, 0, &mut context, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(out, moved.as_bytes());
        assert_eq!(store.get(&key, 0), Some(CacheValueView::Bytes(b"imported")));
    }

    #[test]
    fn migrating_source_returns_tryagain_for_mixed_multikey_residency() {
        let mut map = CacheSlotMap::new_local(1, 1).unwrap();
        let key_a = b"a{migration}".to_vec();
        let key_b = b"b{migration}".to_vec();
        let slot = redis_slot(&key_a);
        assert_eq!(redis_slot(&key_b), slot);
        let source = map.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 9,
            shard: 0,
        };
        map.begin_migration(1, slot, source, target).unwrap();

        let (channels, _inboxes) = CacheDispatchChannels::new(1, 8).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(source, CacheAdvertisedEndpoint::new("127.0.0.1", 7000));
        endpoints.insert(target, CacheAdvertisedEndpoint::new("cache-nine", 7000));
        let dispatcher = CacheDispatcher::new(1, 0, map, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);
        let command = frame(&[b"MGET", &key_a, &key_b]);
        let mut store = CacheStore::new();
        store.set_bytes(&key_a, b"value", None, 0);
        let mut out = Vec::new();

        dispatcher
            .dispatch_frame(&mut store, &command, 0, &mut out)
            .unwrap()
            .unwrap();

        assert_eq!(
            out,
            b"-TRYAGAIN Multiple keys request during rehashing of slot\r\n"
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
    fn cluster_topology_commands_execute_locally_from_routing_snapshot() {
        let map = CacheSlotMap::new_local(1, 1).unwrap();
        let owner = map.owner_for_slot(0).unwrap();
        let (channels, _inboxes) = CacheDispatchChannels::new(1, 8).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(owner, CacheAdvertisedEndpoint::new("cache.local", 7000));
        let dispatcher = CacheDispatcher::new(1, 0, map, channels)
            .unwrap()
            .with_endpoints(endpoints);
        let mut store = CacheStore::new();
        let mut out = Vec::new();

        let slots = frame(&[b"CLUSTER", b"SLOTS"]);
        let outcome = dispatcher
            .dispatch_frame(&mut store, &slots, 0, &mut out)
            .unwrap()
            .unwrap();

        assert!(matches!(outcome, CacheDispatchOutcome::Executed { .. }));
        assert!(out.starts_with(b"*1\r\n"));
        assert!(out
            .windows(b"cache.local".len())
            .any(|window| window == b"cache.local"));
        assert!(store.is_empty());
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
