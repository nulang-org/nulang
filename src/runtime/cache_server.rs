//! Dedicated readiness reactor for the RESP cache tier.
//!
//! The cache server is intentionally separate from the actor scheduler. Each
//! physical cache shard owns its listener, connections, CacheStore, and expiry
//! work on one thread. Cluster-aware clients are redirected to the owning shard
//! before execution, so the steady-state GET/SET path stays thread-local.

#![cfg(feature = "cache-server")]

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};
use parking_lot::Mutex;

use super::cache::{
    CacheStore, CacheTransferBatch, CacheTransferCursor, CacheTransferEntry, CacheTransferFinalize,
    CacheTransferImport, CacheTransferImportTracker,
};
use super::cache_cluster::{CacheAdvertisedEndpoint, CacheEndpointMap, CacheRoutingMode};
use super::cache_dispatch::{
    CacheDispatchChannels, CacheDispatchConfigError, CacheDispatchWake, CacheDispatcher,
    CacheShardInbox,
};
use super::cache_pipeline::{CachePipelineError, CacheResponsePipeline};
use super::cache_routing::{CachePlacementError, CacheShardOwner, CacheSlotMap};
use super::cache_transport::{
    CacheServiceTransportEndpoint, CacheServiceTransportSender, CacheTransportBridgeError,
    CacheTransportInbound, CacheTransportMessage, CacheTransportOutbound,
};
use super::cluster::NodeId;
use super::resp::{parse_command, RespParseError};
use super::resp_cache::{command_slot, execute_command, RespCommandSlot};

const LISTENER_TOKEN: Token = Token(0);
const WAKE_TOKEN: Token = Token(1);
const FIRST_CONNECTION_TOKEN: usize = 2;
const CACHE_NETWORK_EVENT_CAPACITY: usize = 1024;
const CACHE_NETWORK_MAX_BATCH: usize = 64;
const CACHE_NETWORK_IDLE_SLEEP: Duration = Duration::from_millis(1);
const CACHE_NETWORK_CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
const CACHE_NETWORK_DEDUPE_ENTRIES: usize = 4_096;
const CACHE_NETWORK_DEDUPE_RETENTION: Duration = Duration::from_secs(120);
const CACHE_NETWORK_PENDING_ENTRIES: usize = 4_096;
const CACHE_NETWORK_RETRY_INITIAL: Duration = Duration::from_millis(10);
const CACHE_NETWORK_RETRY_MAX: Duration = Duration::from_millis(250);
const CACHE_NETWORK_RETRY_ATTEMPTS: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheNetworkTimeoutOperation {
    Command {
        request_id: u64,
        placement_epoch: u64,
        slot: u16,
    },
    Transfer {
        transfer_id: u64,
        placement_epoch: u64,
        slot: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheNetworkTimeout {
    pub peer: NodeId,
    pub attempts: u8,
    /// A command timeout means the execution outcome is unknown: the remote
    /// mutation may have committed while every response was lost. A transfer
    /// timeout is safer because source finalization still requires its matching
    /// application-level TransferAck.
    pub operation: CacheNetworkTimeoutOperation,
}

#[derive(Debug, Clone)]
pub struct CacheServerConfig {
    pub max_connections: usize,
    pub max_input_buffer: usize,
    pub max_output_buffer: usize,
    pub max_pipeline_depth: usize,
    pub events_capacity: usize,
    pub max_inbox_batch: usize,
    pub control_queue_capacity: usize,
    pub max_control_batch: usize,
    pub expiry_sweep_interval: Duration,
    pub max_expiry_items_per_sweep: usize,
}

impl Default for CacheServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 16_384,
            max_input_buffer: 1024 * 1024,
            max_output_buffer: 8 * 1024 * 1024,
            max_pipeline_depth: 1024,
            events_capacity: 1024,
            max_inbox_batch: 256,
            control_queue_capacity: 64,
            max_control_batch: 32,
            expiry_sweep_interval: Duration::from_millis(100),
            max_expiry_items_per_sweep: 4096,
        }
    }
}

#[derive(Debug)]
pub enum CacheServerError {
    Io(io::Error),
    DispatchConfig(CacheDispatchConfigError),
    RedirectModeRequired,
    InvalidConfig(&'static str),
}

impl From<io::Error> for CacheServerError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<CacheDispatchConfigError> for CacheServerError {
    fn from(value: CacheDispatchConfigError) -> Self {
        Self::DispatchConfig(value)
    }
}

/// Shared monotonic time origin for cache shards in one process.
///
/// Local cross-shard commands carry now_ms in the bounded request message, so
/// every local shard must use the same origin. Remote nodes intentionally do
/// not exchange these process-relative timestamps.
#[derive(Debug, Clone)]
pub struct CacheServerClock {
    origin: Arc<Instant>,
}

impl CacheServerClock {
    pub fn new() -> Self {
        Self {
            origin: Arc::new(Instant::now()),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
}

impl Default for CacheServerClock {
    fn default() -> Self {
        Self::new()
    }
}

struct CachePlacementPublisher {
    published_epoch: AtomicU64,
    snapshot: Mutex<CacheSlotMap>,
}

impl CachePlacementPublisher {
    fn new(snapshot: CacheSlotMap) -> Self {
        Self {
            published_epoch: AtomicU64::new(snapshot.epoch()),
            snapshot: Mutex::new(snapshot),
        }
    }

    fn published_epoch(&self) -> u64 {
        self.published_epoch.load(Ordering::Acquire)
    }

    fn publish(&self, next: CacheSlotMap) -> Result<(), CacheServiceError> {
        let proposed = next.epoch();
        let mut snapshot = self.snapshot.lock();
        let current = snapshot.epoch();
        if proposed <= current {
            return Err(CacheServiceError::StalePlacementEpoch { current, proposed });
        }
        *snapshot = next;
        self.published_epoch.store(proposed, Ordering::Release);
        Ok(())
    }

    fn snapshot_if_newer(&self, installed_epoch: u64) -> Option<CacheSlotMap> {
        if self.published_epoch() <= installed_epoch {
            return None;
        }
        let snapshot = self.snapshot.lock();
        (snapshot.epoch() > installed_epoch).then(|| snapshot.clone())
    }

    fn snapshot(&self) -> CacheSlotMap {
        self.snapshot.lock().clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheRemoteControlError {
    TopologyChanged {
        installed_epoch: u64,
        requested_epoch: u64,
    },
    OwnerMismatch,
    InvalidFrame,
    Parse(RespParseError),
}

enum CacheShardControlRequest {
    ExecuteRemoteCommand {
        placement_epoch: u64,
        slot: u16,
        frame: Vec<u8>,
        reply: SyncSender<Result<Vec<u8>, CacheRemoteControlError>>,
    },
    ImportRemoteBatch {
        placement_epoch: u64,
        batch: CacheTransferBatch,
        reply: SyncSender<Result<Vec<CacheTransferImport>, CacheRemoteControlError>>,
    },
    Export {
        slot: u16,
        cursor: Option<CacheTransferCursor>,
        max_entries: usize,
        reply: SyncSender<CacheTransferBatch>,
    },
    Import {
        batch: CacheTransferBatch,
        reply: SyncSender<Vec<CacheTransferImport>>,
    },
    Finalize {
        entries: Vec<CacheTransferEntry>,
        reply: SyncSender<Vec<CacheTransferFinalize>>,
    },
    CountSlot {
        slot: u16,
        reply: SyncSender<usize>,
    },
    ClearImportTracker {
        slot: u16,
        reply: SyncSender<()>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheLocalTransferReport {
    pub slot: u16,
    pub source_shard: u16,
    pub target_shard: u16,
    pub next_cursor: Option<CacheTransferCursor>,
    pub scanned_slots: usize,
    pub payload_bytes: usize,
    pub exported_entries: usize,
    pub imported: usize,
    pub already_imported: usize,
    pub expired_in_transit: usize,
    pub conflicts: usize,
    pub wrong_slot: usize,
    pub finalized_removed: usize,
    pub finalized_absent: usize,
    pub stale_source_versions: usize,
    pub source_remaining: usize,
}

impl CacheLocalTransferReport {
    pub fn source_drained(&self) -> bool {
        self.source_remaining == 0
    }

    pub fn restart_scan_required(&self) -> bool {
        self.stale_source_versions != 0 || self.conflicts != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRemoteTransferPending {
    pub transfer_id: u64,
    pub placement_epoch: u64,
    pub source: CacheShardOwner,
    pub target: CacheShardOwner,
    pub batch: CacheTransferBatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRemoteTransferReport {
    pub transfer_id: u64,
    pub slot: u16,
    pub source: CacheShardOwner,
    pub target: CacheShardOwner,
    pub next_cursor: Option<CacheTransferCursor>,
    pub exported_entries: usize,
    pub imported: usize,
    pub already_imported: usize,
    pub expired_in_transit: usize,
    pub conflicts: usize,
    pub wrong_slot: usize,
    pub finalized_removed: usize,
    pub finalized_absent: usize,
    pub stale_source_versions: usize,
    pub source_remaining: usize,
}

impl CacheRemoteTransferReport {
    pub fn source_drained(&self) -> bool {
        self.source_remaining == 0
    }

    pub fn restart_scan_required(&self) -> bool {
        self.stale_source_versions != 0 || self.conflicts != 0 || self.wrong_slot != 0
    }
}

#[derive(Clone)]
pub struct CacheShardServerControl {
    shard: u16,
    shutdown: Arc<AtomicBool>,
    waker: Arc<Waker>,
    applied_placement_epoch: Arc<AtomicU64>,
    control_tx: SyncSender<CacheShardControlRequest>,
}

impl CacheShardServerControl {
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.waker.wake();
    }

    pub fn wake(&self) {
        let _ = self.waker.wake();
    }

    pub fn placement_epoch(&self) -> u64 {
        self.applied_placement_epoch.load(Ordering::Acquire)
    }

    fn request_control(&self, request: CacheShardControlRequest) -> Result<(), CacheServiceError> {
        match self.control_tx.try_send(request) {
            Ok(()) => {
                let _ = self.waker.wake();
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(CacheServiceError::ControlQueueFull(self.shard)),
            Err(TrySendError::Disconnected(_)) => {
                Err(CacheServiceError::ControlDisconnected(self.shard))
            }
        }
    }
}

impl CacheDispatchWake for Waker {
    fn wake(&self) {
        let _ = Waker::wake(self);
    }
}

#[derive(Debug, Clone)]
pub struct CacheServiceShardConfig {
    pub bind_addr: SocketAddr,
    pub advertised_host: String,
    pub advertised_port: Option<u16>,
    pub cpu: Option<usize>,
}

impl CacheServiceShardConfig {
    pub fn new(bind_addr: SocketAddr, advertised_host: impl Into<String>) -> Self {
        Self {
            bind_addr,
            advertised_host: advertised_host.into(),
            advertised_port: None,
            cpu: None,
        }
    }

    /// Override the port published in MOVED / CLUSTER topology responses.
    ///
    /// By default the service publishes the listener's actual bound port,
    /// which also makes bind port 0 useful for tests and dynamic allocation.
    pub fn with_advertised_port(mut self, port: u16) -> Self {
        self.advertised_port = Some(port);
        self
    }

    /// Best-effort pin of this shard reactor to a logical CPU.
    pub fn pin_to_cpu(mut self, cpu: usize) -> Self {
        self.cpu = Some(cpu);
        self
    }
}

#[derive(Debug)]
pub enum CacheServiceError {
    Io(io::Error),
    Placement(CachePlacementError),
    DispatchConfig(CacheDispatchConfigError),
    NoShards,
    TooManyShards,
    MissingLocalShard(u16),
    MissingEndpoint(CacheShardOwner),
    StalePlacementEpoch {
        current: u64,
        proposed: u64,
    },
    InvalidTransferShard {
        shard: u16,
        shard_count: u16,
    },
    TransferSameShard(u16),
    InvalidTransferBatchSize,
    ControlQueueFull(u16),
    ControlDisconnected(u16),
    TransportUnavailable,
    TransportBridge(CacheTransportBridgeError),
    NetworkEventDisconnected,
    NetworkRetryQueueFull,
    NetworkRequestIdInUse,
    RemoteTransferTargetMustBeRemote,
    RemoteTransferNotActive(u16),
    RemoteTransferAckMismatch,
    ShardServer {
        shard: u16,
        source: CacheServerError,
    },
    ThreadPanicked(u16),
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
        Self::DispatchConfig(value)
    }
}

impl From<CacheTransportBridgeError> for CacheServiceError {
    fn from(value: CacheTransportBridgeError) -> Self {
        Self::TransportBridge(value)
    }
}

pub struct CacheServiceBuilder {
    local_node_id: u64,
    placement: CacheSlotMap,
    endpoints: CacheEndpointMap,
    shards: Vec<CacheServiceShardConfig>,
    queue_capacity: usize,
    server_config: CacheServerConfig,
    transport_endpoint: Option<CacheServiceTransportEndpoint>,
}

impl CacheServiceBuilder {
    pub fn new(local_node_id: u64, placement: CacheSlotMap) -> Self {
        Self {
            local_node_id,
            placement,
            endpoints: CacheEndpointMap::new(),
            shards: Vec::new(),
            queue_capacity: 1024,
            server_config: CacheServerConfig::default(),
            transport_endpoint: None,
        }
    }

    /// Convenience builder for a process that initially owns all Redis slots.
    pub fn local(local_node_id: u64, shard_count: u16) -> Result<Self, CacheServiceError> {
        Ok(Self::new(
            local_node_id,
            CacheSlotMap::new_local(local_node_id, shard_count)?,
        ))
    }

    pub fn with_queue_capacity(mut self, queue_capacity: usize) -> Self {
        self.queue_capacity = queue_capacity;
        self
    }

    pub fn with_server_config(mut self, server_config: CacheServerConfig) -> Self {
        self.server_config = server_config;
        self
    }

    pub fn with_transport_endpoint(mut self, endpoint: CacheServiceTransportEndpoint) -> Self {
        self.transport_endpoint = Some(endpoint);
        self
    }

    /// Register an already-known endpoint, normally for a remote shard.
    pub fn with_endpoint(
        mut self,
        owner: CacheShardOwner,
        endpoint: CacheAdvertisedEndpoint,
    ) -> Self {
        self.endpoints.insert(owner, endpoint);
        self
    }

    /// Add one local physical shard. Vector order defines the local shard id.
    pub fn with_shard(mut self, shard: CacheServiceShardConfig) -> Self {
        self.shards.push(shard);
        self
    }

    pub fn build(self) -> Result<CacheService, CacheServiceError> {
        if self.shards.is_empty() {
            return Err(CacheServiceError::NoShards);
        }
        let shard_count =
            u16::try_from(self.shards.len()).map_err(|_| CacheServiceError::TooManyShards)?;

        for range in self.placement.slot_ranges() {
            if range.owner.node_id == self.local_node_id && range.owner.shard >= shard_count {
                return Err(CacheServiceError::MissingLocalShard(range.owner.shard));
            }
        }

        let (channels, inboxes) = CacheDispatchChannels::new(shard_count, self.queue_capacity)?;

        // Bind every listener before constructing any dispatcher. This makes
        // actual port-0 allocations available to every shard's topology view.
        let mut endpoints = self.endpoints;
        let mut reserved = Vec::with_capacity(self.shards.len());
        for (index, shard) in self.shards.iter().enumerate() {
            let listener = TcpListener::bind(shard.bind_addr)?;
            let local_addr = listener.local_addr()?;
            let advertised_port = shard.advertised_port.unwrap_or(local_addr.port());
            let owner = CacheShardOwner {
                node_id: self.local_node_id,
                shard: index as u16,
            };
            endpoints.insert(
                owner,
                CacheAdvertisedEndpoint::new(&shard.advertised_host, advertised_port),
            );
            reserved.push((listener, local_addr, shard.cpu));
        }

        validate_placement_endpoints(&self.placement, &endpoints)?;

        let placement_publisher = Arc::new(CachePlacementPublisher::new(self.placement.clone()));
        let clock = CacheServerClock::new();
        let mut servers = Vec::with_capacity(self.shards.len());
        let mut local_addrs = Vec::with_capacity(self.shards.len());
        let mut cpus = Vec::with_capacity(self.shards.len());

        for (index, ((listener, local_addr, cpu), inbox)) in
            reserved.into_iter().zip(inboxes.into_iter()).enumerate()
        {
            let shard = index as u16;
            let dispatcher = CacheDispatcher::new(
                self.local_node_id,
                shard,
                self.placement.clone(),
                channels.clone(),
            )?
            .with_cluster_redirects(endpoints.clone());

            let server = CacheShardServer::from_listener(
                listener,
                dispatcher,
                inbox,
                CacheStore::new(),
                self.server_config.clone(),
                clock.clone(),
                Some(placement_publisher.clone()),
            )
            .map_err(|source| CacheServiceError::ShardServer { shard, source })?;

            servers.push(server);
            local_addrs.push(local_addr);
            cpus.push(cpu);
        }

        Ok(CacheService {
            local_node_id: self.local_node_id,
            servers,
            local_addrs,
            endpoints,
            cpus,
            placement_publisher,
            transport_endpoint: self.transport_endpoint,
        })
    }
}

pub struct CacheService {
    local_node_id: u64,
    servers: Vec<CacheShardServer>,
    local_addrs: Vec<SocketAddr>,
    endpoints: CacheEndpointMap,
    cpus: Vec<Option<usize>>,
    placement_publisher: Arc<CachePlacementPublisher>,
    transport_endpoint: Option<CacheServiceTransportEndpoint>,
}

impl CacheService {
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    pub fn endpoints(&self) -> &CacheEndpointMap {
        &self.endpoints
    }

    pub fn start(self) -> Result<CacheServiceHandle, CacheServiceError> {
        let controls: Vec<_> = self.servers.iter().map(CacheShardServer::control).collect();
        let transport_sender = self
            .transport_endpoint
            .as_ref()
            .map(CacheServiceTransportEndpoint::sender);
        let network_shutdown = Arc::new(AtomicBool::new(false));
        let network_retry = Arc::new(Mutex::new(CacheNetworkRetryState::new(
            CACHE_NETWORK_PENDING_ENTRIES,
        )));
        let (network_event_tx, network_event_rx) = mpsc::sync_channel(CACHE_NETWORK_EVENT_CAPACITY);
        let (network_timeout_tx, network_timeout_rx) = mpsc::channel();
        let mut threads: Vec<(u16, JoinHandle<Result<(), CacheServerError>>)> =
            Vec::with_capacity(self.servers.len());

        for (index, (mut server, cpu)) in self
            .servers
            .into_iter()
            .zip(self.cpus.into_iter())
            .enumerate()
        {
            let shard = index as u16;
            let spawn = thread::Builder::new()
                .name(format!("nulang-cache-{shard}"))
                .spawn(move || {
                    if let Some(cpu) = cpu {
                        let _ = super::scheduler::pin_current_thread_to_cpu(cpu);
                    }
                    server.run()
                });

            match spawn {
                Ok(handle) => threads.push((shard, handle)),
                Err(error) => {
                    for control in &controls {
                        control.shutdown();
                    }
                    for (_, handle) in threads {
                        let _ = handle.join();
                    }
                    return Err(CacheServiceError::Io(error));
                }
            }
        }

        let has_transport = transport_sender.is_some();
        let network_thread = if let Some(endpoint) = self.transport_endpoint {
            let coordinator_controls = controls.clone();
            let placement_publisher = self.placement_publisher.clone();
            let shutdown = network_shutdown.clone();
            let retry_state = network_retry.clone();
            let local_node_id = self.local_node_id;
            match thread::Builder::new()
                .name("nulang-cache-network".to_string())
                .spawn(move || {
                    run_cache_network_coordinator(
                        local_node_id,
                        endpoint,
                        coordinator_controls,
                        placement_publisher,
                        network_event_tx,
                        network_timeout_tx,
                        retry_state,
                        shutdown,
                    );
                }) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    network_shutdown.store(true, Ordering::Release);
                    for control in &controls {
                        control.shutdown();
                    }
                    for (_, handle) in threads {
                        let _ = handle.join();
                    }
                    return Err(CacheServiceError::Io(error));
                }
            }
        } else {
            None
        };

        Ok(CacheServiceHandle {
            local_node_id: self.local_node_id,
            controls,
            threads,
            local_addrs: self.local_addrs,
            endpoints: self.endpoints,
            placement_publisher: self.placement_publisher,
            transport_sender,
            network_events: has_transport.then_some(network_event_rx),
            network_timeouts: has_transport.then_some(network_timeout_rx),
            network_retry: has_transport.then_some(network_retry),
            network_thread,
            network_shutdown,
        })
    }
}

pub struct CacheServiceHandle {
    local_node_id: u64,
    controls: Vec<CacheShardServerControl>,
    threads: Vec<(u16, JoinHandle<Result<(), CacheServerError>>)>,
    local_addrs: Vec<SocketAddr>,
    endpoints: CacheEndpointMap,
    placement_publisher: Arc<CachePlacementPublisher>,
    transport_sender: Option<CacheServiceTransportSender>,
    network_events: Option<Receiver<CacheTransportInbound>>,
    network_timeouts: Option<Receiver<CacheNetworkTimeout>>,
    network_retry: Option<Arc<Mutex<CacheNetworkRetryState>>>,
    network_thread: Option<JoinHandle<()>>,
    network_shutdown: Arc<AtomicBool>,
}

impl CacheServiceHandle {
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    pub fn endpoints(&self) -> &CacheEndpointMap {
        &self.endpoints
    }

    pub fn published_placement_epoch(&self) -> u64 {
        self.placement_publisher.published_epoch()
    }

    pub fn shard_placement_epochs(&self) -> Vec<u64> {
        self.controls
            .iter()
            .map(CacheShardServerControl::placement_epoch)
            .collect()
    }

    pub fn send_network_message(
        &self,
        to_node: NodeId,
        message: CacheTransportMessage,
    ) -> Result<(), CacheServiceError> {
        let sender = self
            .transport_sender
            .as_ref()
            .ok_or(CacheServiceError::TransportUnavailable)?;
        message
            .validate_sender(NodeId(self.local_node_id))
            .map_err(|_| CacheServiceError::TransportUnavailable)?;

        let outbound = CacheTransportOutbound { to_node, message };
        let retry_key = cache_pending_key(outbound.to_node, &outbound.message);
        if let Some(key) = retry_key {
            let retry = self
                .network_retry
                .as_ref()
                .ok_or(CacheServiceError::TransportUnavailable)?;
            let mut retry = retry.lock();
            retry.register(key, outbound.clone())?;
        }

        match sender.try_send(outbound) {
            Ok(()) | Err(CacheTransportBridgeError::OutboundFull) => Ok(()),
            Err(error) => {
                if let (Some(key), Some(retry)) = (retry_key, self.network_retry.as_ref()) {
                    retry.lock().remove(key);
                }
                Err(CacheServiceError::TransportBridge(error))
            }
        }
    }

    /// Receive an application-level remote command response or transfer ACK.
    ///
    /// Transport-level NUL0 ACKs are consumed by Runtime and never appear
    /// here.
    pub fn try_recv_network_event(
        &self,
    ) -> Result<Option<CacheTransportInbound>, CacheServiceError> {
        let receiver = self
            .network_events
            .as_ref()
            .ok_or(CacheServiceError::TransportUnavailable)?;
        match receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(CacheServiceError::NetworkEventDisconnected),
        }
    }

    /// Receive a terminal retry outcome.
    ///
    /// Command timeouts are deliberately not converted into RESP errors because
    /// execution may have happened remotely before the reply was lost. Transfer
    /// timeouts never finalize the source batch and are therefore safe to retry
    /// later with a fresh controller decision.
    pub fn try_recv_network_timeout(
        &self,
    ) -> Result<Option<CacheNetworkTimeout>, CacheServiceError> {
        let receiver = self
            .network_timeouts
            .as_ref()
            .ok_or(CacheServiceError::TransportUnavailable)?;
        match receiver.try_recv() {
            Ok(timeout) => Ok(Some(timeout)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(CacheServiceError::NetworkEventDisconnected),
        }
    }

    /// Publish a newer immutable placement snapshot to every local reactor.
    ///
    /// Publication itself is a cold-path mutex operation. Reactors notice the
    /// newer epoch on their Mio wake path, clone it once into their
    /// thread-local dispatcher, and then return to lock-free indexed routing.
    pub fn install_placement(&self, placement: CacheSlotMap) -> Result<(), CacheServiceError> {
        validate_placement_endpoints(&placement, &self.endpoints)?;
        self.placement_publisher.publish(placement)?;
        for control in &self.controls {
            control.wake();
        }
        Ok(())
    }

    /// Export one bounded source batch and send it to a remote migration target.
    ///
    /// Completion is application-level: callers wait for the matching
    /// TransferAck via try_recv_network_event and then call
    /// complete_remote_slot_batch. The source copy is never deleted merely
    /// because the NUL0 transport acknowledged packet receipt.
    pub fn send_remote_slot_batch(
        &self,
        source_shard: u16,
        target: CacheShardOwner,
        slot: u16,
        cursor: Option<CacheTransferCursor>,
        max_entries: usize,
        transfer_id: u64,
    ) -> Result<CacheRemoteTransferPending, CacheServiceError> {
        if max_entries == 0 {
            return Err(CacheServiceError::InvalidTransferBatchSize);
        }
        if target.node_id == self.local_node_id {
            return Err(CacheServiceError::RemoteTransferTargetMustBeRemote);
        }

        let placement = self.placement_publisher.snapshot();
        let source = CacheShardOwner {
            node_id: self.local_node_id,
            shard: source_shard,
        };
        let Some(migration) = placement.migration_for_slot(slot) else {
            return Err(CacheServiceError::RemoteTransferNotActive(slot));
        };
        if migration.source != source || migration.target != target {
            return Err(CacheServiceError::RemoteTransferNotActive(slot));
        }

        let source_control = self.transfer_control(source_shard)?;
        let (export_tx, export_rx) = mpsc::sync_channel(1);
        source_control.request_control(CacheShardControlRequest::Export {
            slot,
            cursor,
            max_entries,
            reply: export_tx,
        })?;
        let batch = export_rx
            .recv()
            .map_err(|_| CacheServiceError::ControlDisconnected(source_shard))?;

        let pending = CacheRemoteTransferPending {
            transfer_id,
            placement_epoch: placement.epoch(),
            source,
            target,
            batch,
        };
        self.send_network_message(
            NodeId(target.node_id),
            CacheTransportMessage::TransferBatch {
                transfer_id,
                placement_epoch: pending.placement_epoch,
                source,
                target,
                batch: pending.batch.clone(),
            },
        )?;
        Ok(pending)
    }

    /// Retry a previously exported remote transfer batch without re-exporting.
    ///
    /// The transfer id and payload are preserved exactly. The target's
    /// authenticated dedupe cache therefore replays the original application
    /// ACK instead of importing the batch a second time.
    pub fn retry_remote_slot_batch(
        &self,
        pending: &CacheRemoteTransferPending,
    ) -> Result<(), CacheServiceError> {
        let placement = self.placement_publisher.snapshot();
        if placement.epoch() != pending.placement_epoch {
            return Err(CacheServiceError::RemoteTransferNotActive(
                pending.batch.slot,
            ));
        }
        let Some(migration) = placement.migration_for_slot(pending.batch.slot) else {
            return Err(CacheServiceError::RemoteTransferNotActive(
                pending.batch.slot,
            ));
        };
        if migration.source != pending.source || migration.target != pending.target {
            return Err(CacheServiceError::RemoteTransferNotActive(
                pending.batch.slot,
            ));
        }

        self.send_network_message(
            NodeId(pending.target.node_id),
            CacheTransportMessage::TransferBatch {
                transfer_id: pending.transfer_id,
                placement_epoch: pending.placement_epoch,
                source: pending.source,
                target: pending.target,
                batch: pending.batch.clone(),
            },
        )
    }

    /// Apply a matching remote TransferAck and generation-fence source deletion.
    pub fn complete_remote_slot_batch(
        &self,
        pending: &CacheRemoteTransferPending,
        event: &CacheTransportInbound,
    ) -> Result<CacheRemoteTransferReport, CacheServiceError> {
        let CacheTransportMessage::TransferAck {
            transfer_id,
            placement_epoch,
            source,
            target,
            slot,
            results,
        } = &event.message
        else {
            return Err(CacheServiceError::RemoteTransferAckMismatch);
        };

        if event.from_node.0 != pending.target.node_id
            || *transfer_id != pending.transfer_id
            || *placement_epoch != pending.placement_epoch
            || *source != pending.source
            || *target != pending.target
            || *slot != pending.batch.slot
            || results.len() != pending.batch.entries.len()
        {
            return Err(CacheServiceError::RemoteTransferAckMismatch);
        }

        let mut imported = 0;
        let mut already_imported = 0;
        let mut expired_in_transit = 0;
        let mut conflicts = 0;
        let mut wrong_slot = 0;
        let mut finalize_entries = Vec::new();

        for (entry, result) in pending.batch.entries.iter().zip(results.iter().copied()) {
            match result {
                CacheTransferImport::Imported => {
                    imported += 1;
                    finalize_entries.push(entry.clone());
                }
                CacheTransferImport::AlreadyImported => {
                    already_imported += 1;
                    finalize_entries.push(entry.clone());
                }
                CacheTransferImport::ExpiredInTransit => {
                    expired_in_transit += 1;
                    finalize_entries.push(entry.clone());
                }
                CacheTransferImport::Conflict => conflicts += 1,
                CacheTransferImport::WrongSlot => wrong_slot += 1,
            }
        }

        let source_control = self.transfer_control(pending.source.shard)?;
        let mut finalized_removed = 0;
        let mut finalized_absent = 0;
        let mut stale_source_versions = 0;
        if !finalize_entries.is_empty() {
            let (finalize_tx, finalize_rx) = mpsc::sync_channel(1);
            source_control.request_control(CacheShardControlRequest::Finalize {
                entries: finalize_entries,
                reply: finalize_tx,
            })?;
            for result in finalize_rx
                .recv()
                .map_err(|_| CacheServiceError::ControlDisconnected(pending.source.shard))?
            {
                match result {
                    CacheTransferFinalize::Removed => finalized_removed += 1,
                    CacheTransferFinalize::AlreadyAbsent => finalized_absent += 1,
                    CacheTransferFinalize::StaleVersion => stale_source_versions += 1,
                }
            }
        }

        let (count_tx, count_rx) = mpsc::sync_channel(1);
        source_control.request_control(CacheShardControlRequest::CountSlot {
            slot: pending.batch.slot,
            reply: count_tx,
        })?;
        let source_remaining = count_rx
            .recv()
            .map_err(|_| CacheServiceError::ControlDisconnected(pending.source.shard))?;

        Ok(CacheRemoteTransferReport {
            transfer_id: pending.transfer_id,
            slot: pending.batch.slot,
            source: pending.source,
            target: pending.target,
            next_cursor: pending.batch.next_cursor,
            exported_entries: pending.batch.entries.len(),
            imported,
            already_imported,
            expired_in_transit,
            conflicts,
            wrong_slot,
            finalized_removed,
            finalized_absent,
            stale_source_versions,
            source_remaining,
        })
    }

    /// Move one bounded batch of a migrating logical slot between local shards.
    ///
    /// Source export, target import, source finalize, and progress counting all
    /// execute on the owning reactor threads. The service handle only
    /// orchestrates the fenced request/reply protocol.
    pub fn transfer_local_slot_batch(
        &self,
        source_shard: u16,
        target_shard: u16,
        slot: u16,
        cursor: Option<CacheTransferCursor>,
        max_entries: usize,
    ) -> Result<CacheLocalTransferReport, CacheServiceError> {
        if max_entries == 0 {
            return Err(CacheServiceError::InvalidTransferBatchSize);
        }
        if source_shard == target_shard {
            return Err(CacheServiceError::TransferSameShard(source_shard));
        }

        let source = self.transfer_control(source_shard)?;
        let target = self.transfer_control(target_shard)?;

        let (export_tx, export_rx) = mpsc::sync_channel(1);
        source.request_control(CacheShardControlRequest::Export {
            slot,
            cursor,
            max_entries,
            reply: export_tx,
        })?;
        let batch = export_rx
            .recv()
            .map_err(|_| CacheServiceError::ControlDisconnected(source_shard))?;

        let exported_entries = batch.entries.len();
        let next_cursor = batch.next_cursor;
        let scanned_slots = batch.scanned_slots;
        let payload_bytes = batch.payload_bytes;

        let (import_tx, import_rx) = mpsc::sync_channel(1);
        target.request_control(CacheShardControlRequest::Import {
            batch: batch.clone(),
            reply: import_tx,
        })?;
        let import_results = import_rx
            .recv()
            .map_err(|_| CacheServiceError::ControlDisconnected(target_shard))?;

        let mut imported = 0;
        let mut already_imported = 0;
        let mut expired_in_transit = 0;
        let mut conflicts = 0;
        let mut wrong_slot = 0;
        let mut finalize_entries = Vec::new();

        for (entry, result) in batch.entries.into_iter().zip(import_results) {
            match result {
                CacheTransferImport::Imported => {
                    imported += 1;
                    finalize_entries.push(entry);
                }
                CacheTransferImport::AlreadyImported => {
                    already_imported += 1;
                    finalize_entries.push(entry);
                }
                CacheTransferImport::ExpiredInTransit => {
                    expired_in_transit += 1;
                    finalize_entries.push(entry);
                }
                CacheTransferImport::Conflict => conflicts += 1,
                CacheTransferImport::WrongSlot => wrong_slot += 1,
            }
        }

        let mut finalized_removed = 0;
        let mut finalized_absent = 0;
        let mut stale_source_versions = 0;
        if !finalize_entries.is_empty() {
            let (finalize_tx, finalize_rx) = mpsc::sync_channel(1);
            source.request_control(CacheShardControlRequest::Finalize {
                entries: finalize_entries,
                reply: finalize_tx,
            })?;
            for result in finalize_rx
                .recv()
                .map_err(|_| CacheServiceError::ControlDisconnected(source_shard))?
            {
                match result {
                    CacheTransferFinalize::Removed => finalized_removed += 1,
                    CacheTransferFinalize::AlreadyAbsent => finalized_absent += 1,
                    CacheTransferFinalize::StaleVersion => stale_source_versions += 1,
                }
            }
        }

        let (count_tx, count_rx) = mpsc::sync_channel(1);
        source.request_control(CacheShardControlRequest::CountSlot {
            slot,
            reply: count_tx,
        })?;
        let source_remaining = count_rx
            .recv()
            .map_err(|_| CacheServiceError::ControlDisconnected(source_shard))?;

        Ok(CacheLocalTransferReport {
            slot,
            source_shard,
            target_shard,
            next_cursor,
            scanned_slots,
            payload_bytes,
            exported_entries,
            imported,
            already_imported,
            expired_in_transit,
            conflicts,
            wrong_slot,
            finalized_removed,
            finalized_absent,
            stale_source_versions,
            source_remaining,
        })
    }

    /// Release target-side replay fences after the migration has committed and
    /// no transfer batch can still arrive for this slot.
    pub fn clear_local_transfer_imports(
        &self,
        target_shard: u16,
        slot: u16,
    ) -> Result<(), CacheServiceError> {
        let target = self.transfer_control(target_shard)?;
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        target.request_control(CacheShardControlRequest::ClearImportTracker {
            slot,
            reply: reply_tx,
        })?;
        reply_rx
            .recv()
            .map_err(|_| CacheServiceError::ControlDisconnected(target_shard))
    }

    fn transfer_control(&self, shard: u16) -> Result<&CacheShardServerControl, CacheServiceError> {
        self.controls
            .get(shard as usize)
            .ok_or(CacheServiceError::InvalidTransferShard {
                shard,
                shard_count: self.controls.len().min(u16::MAX as usize) as u16,
            })
    }

    pub fn request_shutdown(&self) {
        self.network_shutdown.store(true, Ordering::Release);
        for control in &self.controls {
            control.shutdown();
        }
    }

    /// Stop every shard and join every reactor thread.
    ///
    /// All threads are joined even when more than one fails; the first error
    /// is returned after the full service has quiesced.
    pub fn shutdown(mut self) -> Result<(), CacheServiceError> {
        self.request_shutdown();
        self.join_threads()
    }

    fn join_threads(&mut self) -> Result<(), CacheServiceError> {
        let mut first_error = None;
        for (shard, handle) in self.threads.drain(..) {
            let result = match handle.join() {
                Ok(Ok(())) => None,
                Ok(Err(source)) => Some(CacheServiceError::ShardServer { shard, source }),
                Err(_) => Some(CacheServiceError::ThreadPanicked(shard)),
            };
            if first_error.is_none() {
                first_error = result;
            }
        }
        if let Some(handle) = self.network_thread.take() {
            if handle.join().is_err() && first_error.is_none() {
                first_error = Some(CacheServiceError::ThreadPanicked(u16::MAX));
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
        self.request_shutdown();
        let _ = self.join_threads();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CacheNetworkPendingKey {
    Command { peer: u64, request_id: u64 },
    Transfer { peer: u64, transfer_id: u64 },
}

#[derive(Debug, Clone)]
struct CacheNetworkPending {
    outbound: CacheTransportOutbound,
    next_attempt: Instant,
    backoff: Duration,
    attempts: u8,
}

#[derive(Debug)]
struct CacheNetworkRetryState {
    capacity: usize,
    pending: HashMap<CacheNetworkPendingKey, CacheNetworkPending>,
}

impl CacheNetworkRetryState {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            pending: HashMap::with_capacity(capacity),
        }
    }

    fn register(
        &mut self,
        key: CacheNetworkPendingKey,
        outbound: CacheTransportOutbound,
    ) -> Result<(), CacheServiceError> {
        if let Some(existing) = self.pending.get(&key) {
            if existing.outbound == outbound {
                return Ok(());
            }
            return Err(CacheServiceError::NetworkRequestIdInUse);
        }
        if self.pending.len() >= self.capacity {
            return Err(CacheServiceError::NetworkRetryQueueFull);
        }
        self.pending.insert(
            key,
            CacheNetworkPending {
                outbound,
                next_attempt: Instant::now() + CACHE_NETWORK_RETRY_INITIAL,
                backoff: CACHE_NETWORK_RETRY_INITIAL,
                attempts: 1,
            },
        );
        Ok(())
    }

    fn remove(&mut self, key: CacheNetworkPendingKey) {
        self.pending.remove(&key);
    }
}

fn cache_pending_key(
    peer: NodeId,
    message: &CacheTransportMessage,
) -> Option<CacheNetworkPendingKey> {
    match message {
        CacheTransportMessage::CommandRequest { request_id, .. } => {
            Some(CacheNetworkPendingKey::Command {
                peer: peer.0,
                request_id: *request_id,
            })
        }
        CacheTransportMessage::TransferBatch { transfer_id, .. } => {
            Some(CacheNetworkPendingKey::Transfer {
                peer: peer.0,
                transfer_id: *transfer_id,
            })
        }
        _ => None,
    }
}

fn cache_completion_key(
    peer: NodeId,
    message: &CacheTransportMessage,
) -> Option<CacheNetworkPendingKey> {
    match message {
        CacheTransportMessage::CommandResponse { request_id, .. } => {
            Some(CacheNetworkPendingKey::Command {
                peer: peer.0,
                request_id: *request_id,
            })
        }
        CacheTransportMessage::TransferAck { transfer_id, .. } => {
            Some(CacheNetworkPendingKey::Transfer {
                peer: peer.0,
                transfer_id: *transfer_id,
            })
        }
        _ => None,
    }
}

fn cache_timeout_for_pending(pending: &CacheNetworkPending) -> CacheNetworkTimeout {
    let operation = match &pending.outbound.message {
        CacheTransportMessage::CommandRequest {
            request_id,
            placement_epoch,
            slot,
            ..
        } => CacheNetworkTimeoutOperation::Command {
            request_id: *request_id,
            placement_epoch: *placement_epoch,
            slot: *slot,
        },
        CacheTransportMessage::TransferBatch {
            transfer_id,
            placement_epoch,
            batch,
            ..
        } => CacheNetworkTimeoutOperation::Transfer {
            transfer_id: *transfer_id,
            placement_epoch: *placement_epoch,
            slot: batch.slot,
        },
        _ => unreachable!("only request messages enter retry state"),
    };
    CacheNetworkTimeout {
        peer: pending.outbound.to_node,
        attempts: pending.attempts,
        operation,
    }
}

fn retry_cache_network_pending(
    sender: &CacheServiceTransportSender,
    retry: &Arc<Mutex<CacheNetworkRetryState>>,
    timeout_tx: &mpsc::Sender<CacheNetworkTimeout>,
) {
    let now = Instant::now();
    let mut exhausted = Vec::new();
    let mut retry = retry.lock();

    for (key, pending) in retry.pending.iter_mut() {
        if pending.next_attempt > now {
            continue;
        }
        if pending.attempts >= CACHE_NETWORK_RETRY_ATTEMPTS {
            exhausted.push((*key, cache_timeout_for_pending(pending)));
            continue;
        }

        match sender.try_send(pending.outbound.clone()) {
            Ok(()) | Err(CacheTransportBridgeError::OutboundFull) => {
                pending.attempts = pending.attempts.saturating_add(1);
                pending.backoff = pending
                    .backoff
                    .saturating_mul(2)
                    .min(CACHE_NETWORK_RETRY_MAX);
                pending.next_attempt = now + pending.backoff;
            }
            Err(CacheTransportBridgeError::OutboundDisconnected) => {
                pending.attempts = CACHE_NETWORK_RETRY_ATTEMPTS;
                exhausted.push((*key, cache_timeout_for_pending(pending)));
            }
            Err(_) => {
                pending.attempts = pending.attempts.saturating_add(1);
                pending.next_attempt = now + pending.backoff;
            }
        }
    }

    for (key, timeout) in exhausted {
        retry.pending.remove(&key);
        let _ = timeout_tx.send(timeout);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CacheNetworkDedupeKey {
    Command { peer: u64, request_id: u64 },
    Transfer { peer: u64, transfer_id: u64 },
}

#[derive(Debug, Clone)]
struct CacheNetworkDedupeRecord {
    fingerprint: [u8; 32],
    reply: CacheTransportMessage,
    expires_at: Instant,
}

#[derive(Debug)]
struct CacheNetworkDedupe {
    capacity: usize,
    order: VecDeque<CacheNetworkDedupeKey>,
    records: HashMap<CacheNetworkDedupeKey, CacheNetworkDedupeRecord>,
}

impl CacheNetworkDedupe {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::with_capacity(capacity),
            records: HashMap::with_capacity(capacity),
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        loop {
            let Some(key) = self.order.front().copied() else {
                break;
            };
            let expired = self
                .records
                .get(&key)
                .is_none_or(|record| record.expires_at <= now);
            if !expired {
                break;
            }
            self.order.pop_front();
            self.records.remove(&key);
        }
    }

    fn lookup(
        &mut self,
        key: CacheNetworkDedupeKey,
        fingerprint: [u8; 32],
        now: Instant,
    ) -> Result<Option<CacheTransportMessage>, ()> {
        self.prune_expired(now);
        match self.records.get(&key) {
            Some(record) if record.fingerprint == fingerprint => Ok(Some(record.reply.clone())),
            Some(_) => Err(()),
            None => Ok(None),
        }
    }

    fn can_admit(&mut self, now: Instant) -> bool {
        self.prune_expired(now);
        self.records.len() < self.capacity
    }

    fn insert(
        &mut self,
        key: CacheNetworkDedupeKey,
        fingerprint: [u8; 32],
        reply: CacheTransportMessage,
        now: Instant,
    ) {
        if self.records.contains_key(&key) {
            return;
        }
        debug_assert!(
            self.records.len() < self.capacity,
            "cache retry record inserted without admission"
        );
        self.order.push_back(key);
        self.records.insert(
            key,
            CacheNetworkDedupeRecord {
                fingerprint,
                reply,
                expires_at: now + CACHE_NETWORK_DEDUPE_RETENTION,
            },
        );
    }
}

fn cache_message_fingerprint(message: &CacheTransportMessage) -> [u8; 32] {
    match message.to_wire_bytes() {
        Ok(bytes) => *blake3::hash(&bytes).as_bytes(),
        Err(_) => [0u8; 32],
    }
}

fn run_cache_network_coordinator(
    local_node_id: u64,
    endpoint: CacheServiceTransportEndpoint,
    controls: Vec<CacheShardServerControl>,
    placement_publisher: Arc<CachePlacementPublisher>,
    network_event_tx: SyncSender<CacheTransportInbound>,
    network_timeout_tx: mpsc::Sender<CacheNetworkTimeout>,
    retry_state: Arc<Mutex<CacheNetworkRetryState>>,
    shutdown: Arc<AtomicBool>,
) {
    let sender = endpoint.sender();
    let mut placement = placement_publisher.snapshot();
    let mut dedupe = CacheNetworkDedupe::new(CACHE_NETWORK_DEDUPE_ENTRIES);

    while !shutdown.load(Ordering::Acquire) {
        retry_cache_network_pending(&sender, &retry_state, &network_timeout_tx);
        if let Some(next) = placement_publisher.snapshot_if_newer(placement.epoch()) {
            placement = next;
        }

        let mut processed = 0usize;
        while processed < CACHE_NETWORK_MAX_BATCH {
            let inbound = match endpoint.try_recv() {
                Ok(Some(inbound)) => inbound,
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(
                        "nulang-cache: cache service transport receive failed: {:?}",
                        error
                    );
                    return;
                }
            };
            processed += 1;

            if let Some(next) = placement_publisher.snapshot_if_newer(placement.epoch()) {
                placement = next;
            }

            let dedupe_key = match &inbound.message {
                CacheTransportMessage::CommandRequest { request_id, .. } => {
                    Some(CacheNetworkDedupeKey::Command {
                        peer: inbound.from_node.0,
                        request_id: *request_id,
                    })
                }
                CacheTransportMessage::TransferBatch { transfer_id, .. } => {
                    Some(CacheNetworkDedupeKey::Transfer {
                        peer: inbound.from_node.0,
                        transfer_id: *transfer_id,
                    })
                }
                _ => None,
            };
            let fingerprint = cache_message_fingerprint(&inbound.message);

            if let Some(key) = dedupe_key {
                let dedupe_now = Instant::now();
                match dedupe.lookup(key, fingerprint, dedupe_now) {
                    Ok(Some(reply)) => {
                        send_cache_transport_outbound(
                            &sender,
                            CacheTransportOutbound {
                                to_node: inbound.from_node,
                                message: reply,
                            },
                        );
                        continue;
                    }
                    Err(()) => {
                        tracing::warn!(
                            "nulang-cache: rejecting cache request id reuse with different payload from {:?}",
                            inbound.from_node
                        );
                        reject_cache_network_id_reuse(local_node_id, &sender, inbound);
                        continue;
                    }
                    Ok(None) => {
                        if !dedupe.can_admit(dedupe_now) {
                            tracing::warn!(
                                "nulang-cache: retry dedupe window saturated; rejecting new cache operation from {:?}",
                                inbound.from_node
                            );
                            reject_cache_network_saturated(local_node_id, &sender, inbound);
                            continue;
                        }
                    }
                }
            }

            if let Err(error) = inbound.message.validate_for_node(local_node_id, &placement) {
                tracing::warn!(
                    "nulang-cache: rejecting cache envelope from {:?}: {:?}",
                    inbound.from_node,
                    error
                );
                let reply =
                    reject_cache_network_inbound(local_node_id, &sender, inbound.clone(), error);
                if let (Some(key), Some(reply)) = (dedupe_key, reply) {
                    dedupe.insert(key, fingerprint, reply, Instant::now());
                }
                continue;
            }

            if let Some(reply) = handle_cache_network_inbound(
                local_node_id,
                &sender,
                &controls,
                &network_event_tx,
                &retry_state,
                inbound.clone(),
            ) {
                if let Some(key) = dedupe_key {
                    dedupe.insert(key, fingerprint, reply, Instant::now());
                }
            }
        }

        if processed == 0 {
            thread::sleep(CACHE_NETWORK_IDLE_SLEEP);
        }
    }
}

fn handle_cache_network_inbound(
    local_node_id: u64,
    sender: &CacheServiceTransportSender,
    controls: &[CacheShardServerControl],
    network_event_tx: &SyncSender<CacheTransportInbound>,
    retry_state: &Arc<Mutex<CacheNetworkRetryState>>,
    inbound: CacheTransportInbound,
) -> Option<CacheTransportMessage> {
    let from_node = inbound.from_node;
    match inbound.message {
        CacheTransportMessage::CommandRequest {
            request_id,
            placement_epoch,
            slot,
            target,
            frame,
        } => {
            let response =
                execute_remote_command_on_reactor(controls, target, placement_epoch, slot, frame);
            let reply = CacheTransportMessage::CommandResponse {
                request_id,
                placement_epoch,
                slot,
                responder: target,
                response,
            };
            send_cache_transport_outbound(
                sender,
                CacheTransportOutbound {
                    to_node: from_node,
                    message: reply.clone(),
                },
            );
            return Some(reply);
        }
        CacheTransportMessage::TransferBatch {
            transfer_id,
            placement_epoch,
            source,
            target,
            batch,
        } => {
            let results =
                import_remote_batch_on_reactor(controls, target, placement_epoch, batch.clone())
                    .unwrap_or_else(|| vec![CacheTransferImport::Conflict; batch.entries.len()]);
            let reply = CacheTransportMessage::TransferAck {
                transfer_id,
                placement_epoch,
                source,
                target,
                slot: batch.slot,
                results,
            };
            send_cache_transport_outbound(
                sender,
                CacheTransportOutbound {
                    to_node: from_node,
                    message: reply.clone(),
                },
            );
            return Some(reply);
        }
        message @ (CacheTransportMessage::CommandResponse { .. }
        | CacheTransportMessage::TransferAck { .. }) => {
            let completion = cache_completion_key(from_node, &message);
            match network_event_tx.try_send(CacheTransportInbound { from_node, message }) {
                Ok(()) => {
                    if let Some(key) = completion {
                        retry_state.lock().remove(key);
                    }
                }
                Err(TrySendError::Full(_)) => tracing::warn!(
                    "nulang-cache: cache network event queue is full; keeping retry active"
                ),
                Err(TrySendError::Disconnected(_)) => {
                    if let Some(key) = completion {
                        retry_state.lock().remove(key);
                    }
                    tracing::warn!(
                        "nulang-cache: dropping cache network event because receiver disconnected"
                    );
                }
            }
        }
    }

    let _ = local_node_id;
    None
}

fn reject_cache_network_inbound(
    local_node_id: u64,
    sender: &CacheServiceTransportSender,
    inbound: CacheTransportInbound,
    _error: super::cache_transport::CacheTransportValidationError,
) -> Option<CacheTransportMessage> {
    match inbound.message {
        CacheTransportMessage::CommandRequest {
            request_id,
            placement_epoch,
            slot,
            target,
            ..
        } if target.node_id == local_node_id => {
            let reply = CacheTransportMessage::CommandResponse {
                request_id,
                placement_epoch,
                slot,
                responder: target,
                response: b"-TRYAGAIN cache topology changed\r\n".to_vec(),
            };
            send_cache_transport_outbound(
                sender,
                CacheTransportOutbound {
                    to_node: inbound.from_node,
                    message: reply.clone(),
                },
            );
            return Some(reply);
        }
        CacheTransportMessage::TransferBatch {
            transfer_id,
            placement_epoch,
            source,
            target,
            batch,
        } if target.node_id == local_node_id => {
            let reply = CacheTransportMessage::TransferAck {
                transfer_id,
                placement_epoch,
                source,
                target,
                slot: batch.slot,
                results: vec![CacheTransferImport::Conflict; batch.entries.len()],
            };
            send_cache_transport_outbound(
                sender,
                CacheTransportOutbound {
                    to_node: inbound.from_node,
                    message: reply.clone(),
                },
            );
            return Some(reply);
        }
        _ => {}
    }
    None
}

fn reject_cache_network_id_reuse(
    local_node_id: u64,
    sender: &CacheServiceTransportSender,
    inbound: CacheTransportInbound,
) {
    match inbound.message {
        CacheTransportMessage::CommandRequest {
            request_id,
            placement_epoch,
            slot,
            target,
            ..
        } if target.node_id == local_node_id => {
            send_cache_transport_outbound(
                sender,
                CacheTransportOutbound {
                    to_node: inbound.from_node,
                    message: CacheTransportMessage::CommandResponse {
                        request_id,
                        placement_epoch,
                        slot,
                        responder: target,
                        response: b"-ERR cache request id reused with different payload\r\n".to_vec(),
                    },
                },
            );
        }
        CacheTransportMessage::TransferBatch {
            transfer_id,
            placement_epoch,
            source,
            target,
            batch,
        } if target.node_id == local_node_id => {
            send_cache_transport_outbound(
                sender,
                CacheTransportOutbound {
                    to_node: inbound.from_node,
                    message: CacheTransportMessage::TransferAck {
                        transfer_id,
                        placement_epoch,
                        source,
                        target,
                        slot: batch.slot,
                        results: vec![CacheTransferImport::Conflict; batch.entries.len()],
                    },
                },
            );
        }
        _ => {}
    }
}

fn reject_cache_network_saturated(
    local_node_id: u64,
    sender: &CacheServiceTransportSender,
    inbound: CacheTransportInbound,
) {
    match inbound.message {
        CacheTransportMessage::CommandRequest {
            request_id,
            placement_epoch,
            slot,
            target,
            ..
        } if target.node_id == local_node_id => {
            send_cache_transport_outbound(
                sender,
                CacheTransportOutbound {
                    to_node: inbound.from_node,
                    message: CacheTransportMessage::CommandResponse {
                        request_id,
                        placement_epoch,
                        slot,
                        responder: target,
                        response: b"-TRYAGAIN cache retry window saturated\r\n".to_vec(),
                    },
                },
            );
        }
        CacheTransportMessage::TransferBatch {
            transfer_id,
            placement_epoch,
            source,
            target,
            batch,
        } if target.node_id == local_node_id => {
            send_cache_transport_outbound(
                sender,
                CacheTransportOutbound {
                    to_node: inbound.from_node,
                    message: CacheTransportMessage::TransferAck {
                        transfer_id,
                        placement_epoch,
                        source,
                        target,
                        slot: batch.slot,
                        results: vec![CacheTransferImport::Conflict; batch.entries.len()],
                    },
                },
            );
        }
        _ => {}
    }
}

fn execute_remote_command_on_reactor(
    controls: &[CacheShardServerControl],
    target: CacheShardOwner,
    placement_epoch: u64,
    slot: u16,
    frame: Vec<u8>,
) -> Vec<u8> {
    let Some(control) = controls.get(target.shard as usize) else {
        return b"-TRYAGAIN cache target shard unavailable\r\n".to_vec();
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if control
        .request_control(CacheShardControlRequest::ExecuteRemoteCommand {
            placement_epoch,
            slot,
            frame,
            reply: reply_tx,
        })
        .is_err()
    {
        return b"-TRYAGAIN cache target shard busy\r\n".to_vec();
    }

    match reply_rx.recv_timeout(CACHE_NETWORK_CONTROL_TIMEOUT) {
        Ok(Ok(response)) => response,
        Ok(Err(CacheRemoteControlError::Parse(_) | CacheRemoteControlError::InvalidFrame)) => {
            b"-ERR invalid cache transport command\r\n".to_vec()
        }
        Ok(Err(
            CacheRemoteControlError::TopologyChanged { .. }
            | CacheRemoteControlError::OwnerMismatch,
        ))
        | Err(_) => b"-TRYAGAIN cache topology changed\r\n".to_vec(),
    }
}

fn import_remote_batch_on_reactor(
    controls: &[CacheShardServerControl],
    target: CacheShardOwner,
    placement_epoch: u64,
    batch: CacheTransferBatch,
) -> Option<Vec<CacheTransferImport>> {
    let control = controls.get(target.shard as usize)?;
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    control
        .request_control(CacheShardControlRequest::ImportRemoteBatch {
            placement_epoch,
            batch,
            reply: reply_tx,
        })
        .ok()?;
    match reply_rx.recv_timeout(CACHE_NETWORK_CONTROL_TIMEOUT) {
        Ok(Ok(results)) => Some(results),
        Ok(Err(_)) | Err(_) => None,
    }
}

fn send_cache_transport_outbound(
    sender: &CacheServiceTransportSender,
    outbound: CacheTransportOutbound,
) {
    if let Err(error) = sender.try_send(outbound) {
        tracing::warn!(
            "nulang-cache: unable to enqueue cache response/ACK for NUL0: {:?}",
            error
        );
    }
}

struct CacheConnection {
    stream: TcpStream,
    input: Vec<u8>,
    input_start: usize,
    output: Vec<u8>,
    output_start: usize,
    pipeline: CacheResponsePipeline,
    writable_interest: bool,
}

impl CacheConnection {
    fn new(stream: TcpStream, max_pipeline_depth: usize) -> Self {
        Self {
            stream,
            input: Vec::with_capacity(4096),
            input_start: 0,
            output: Vec::with_capacity(4096),
            output_start: 0,
            pipeline: CacheResponsePipeline::new(max_pipeline_depth),
            writable_interest: false,
        }
    }

    fn pending_output(&self) -> usize {
        self.output.len().saturating_sub(self.output_start)
    }

    fn compact_input(&mut self) {
        if self.input_start == 0 {
            return;
        }
        if self.input_start == self.input.len() {
            self.input.clear();
            self.input_start = 0;
            return;
        }
        if self.input_start >= 64 * 1024 || self.input_start * 2 >= self.input.len() {
            self.input.copy_within(self.input_start.., 0);
            self.input.truncate(self.input.len() - self.input_start);
            self.input_start = 0;
        }
    }

    fn compact_output(&mut self) {
        if self.output_start == self.output.len() {
            self.output.clear();
            self.output_start = 0;
        } else if self.output_start >= 64 * 1024 && self.output_start * 2 >= self.output.len() {
            self.output.copy_within(self.output_start.., 0);
            self.output.truncate(self.output.len() - self.output_start);
            self.output_start = 0;
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ReadyEvent {
    token: Token,
    readable: bool,
    writable: bool,
    error: bool,
    read_closed: bool,
    write_closed: bool,
}

/// One physical cache shard and its network reactor.
///
/// CacheShardServer requires redirect routing. A client that reaches the wrong
/// physical owner receives MOVED instead of forcing the server to proxy
/// ordinary traffic across shards.
pub struct CacheShardServer {
    poll: Poll,
    events: Events,
    ready: Vec<ReadyEvent>,
    listener: TcpListener,
    dispatcher: CacheDispatcher,
    inbox: CacheShardInbox,
    store: CacheStore,
    connections: HashMap<Token, CacheConnection>,
    next_connection_token: usize,
    config: CacheServerConfig,
    clock: CacheServerClock,
    shutdown: Arc<AtomicBool>,
    waker: Arc<Waker>,
    next_expiry_sweep: Instant,
    placement_publisher: Option<Arc<CachePlacementPublisher>>,
    applied_placement_epoch: Arc<AtomicU64>,
    control_tx: SyncSender<CacheShardControlRequest>,
    control_rx: Receiver<CacheShardControlRequest>,
    transfer_imports: HashMap<u16, CacheTransferImportTracker>,
}

impl CacheShardServer {
    pub fn bind(
        bind_addr: SocketAddr,
        dispatcher: CacheDispatcher,
        inbox: CacheShardInbox,
        store: CacheStore,
        config: CacheServerConfig,
        clock: CacheServerClock,
    ) -> Result<Self, CacheServerError> {
        let listener = TcpListener::bind(bind_addr)?;
        Self::from_listener(listener, dispatcher, inbox, store, config, clock, None)
    }

    fn from_listener(
        mut listener: TcpListener,
        dispatcher: CacheDispatcher,
        inbox: CacheShardInbox,
        store: CacheStore,
        config: CacheServerConfig,
        clock: CacheServerClock,
        placement_publisher: Option<Arc<CachePlacementPublisher>>,
    ) -> Result<Self, CacheServerError> {
        validate_config(&config)?;
        if dispatcher.routing_mode() != CacheRoutingMode::Redirect {
            return Err(CacheServerError::RedirectModeRequired);
        }
        if dispatcher.local_shard() != inbox.shard() {
            return Err(CacheServerError::DispatchConfig(
                CacheDispatchConfigError::InvalidLocalShard {
                    shard: inbox.shard(),
                    shard_count: dispatcher.shard_count(),
                },
            ));
        }

        let poll = Poll::new()?;
        poll.registry()
            .register(&mut listener, LISTENER_TOKEN, Interest::READABLE)?;

        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN)?);
        dispatcher.install_waker(inbox.shard(), waker.clone())?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let applied_placement_epoch = Arc::new(AtomicU64::new(dispatcher.placement().epoch()));
        let (control_tx, control_rx) = mpsc::sync_channel(config.control_queue_capacity);
        let next_expiry_sweep = Instant::now() + config.expiry_sweep_interval;

        Ok(Self {
            events: Events::with_capacity(config.events_capacity),
            ready: Vec::with_capacity(config.events_capacity),
            poll,
            listener,
            dispatcher,
            inbox,
            store,
            connections: HashMap::new(),
            next_connection_token: FIRST_CONNECTION_TOKEN,
            config,
            clock,
            shutdown,
            waker,
            next_expiry_sweep,
            placement_publisher,
            applied_placement_epoch,
            control_tx,
            control_rx,
            transfer_imports: HashMap::new(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn control(&self) -> CacheShardServerControl {
        CacheShardServerControl {
            shard: self.inbox.shard(),
            shutdown: self.shutdown.clone(),
            waker: self.waker.clone(),
            applied_placement_epoch: self.applied_placement_epoch.clone(),
            control_tx: self.control_tx.clone(),
        }
    }

    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    pub fn store(&self) -> &CacheStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut CacheStore {
        &mut self.store
    }

    pub fn run(&mut self) -> Result<(), CacheServerError> {
        while !self.shutdown.load(Ordering::Acquire) {
            self.poll_once(None)?;
        }
        Ok(())
    }

    /// Drive one reactor iteration. A caller-supplied timeout is capped by the
    /// next expiration sweep deadline so TTL reclamation progresses while idle.
    pub fn poll_once(&mut self, timeout: Option<Duration>) -> Result<(), CacheServerError> {
        let timeout = self.poll_timeout(timeout);
        self.poll.poll(&mut self.events, timeout)?;

        self.ready.clear();
        for event in self.events.iter() {
            self.ready.push(ReadyEvent {
                token: event.token(),
                readable: event.is_readable(),
                writable: event.is_writable(),
                error: event.is_error(),
                read_closed: event.is_read_closed(),
                write_closed: event.is_write_closed(),
            });
        }

        for index in 0..self.ready.len() {
            let event = self.ready[index];
            match event.token {
                LISTENER_TOKEN => self.accept_ready()?,
                WAKE_TOKEN => self.handle_wake()?,
                token => self.connection_ready(token, event)?,
            }
        }

        self.sweep_expired_if_due();
        Ok(())
    }

    fn poll_timeout(&self, requested: Option<Duration>) -> Option<Duration> {
        let until_expiry = self
            .next_expiry_sweep
            .saturating_duration_since(Instant::now());
        match requested {
            Some(requested) => Some(requested.min(until_expiry)),
            None => Some(until_expiry),
        }
    }

    fn sweep_expired_if_due(&mut self) {
        let now = Instant::now();
        if now < self.next_expiry_sweep {
            return;
        }

        self.store
            .purge_expired(self.clock.now_ms(), self.config.max_expiry_items_per_sweep);
        self.next_expiry_sweep = now + self.config.expiry_sweep_interval;
    }

    fn accept_ready(&mut self) -> Result<(), CacheServerError> {
        loop {
            match self.listener.accept() {
                Ok((mut stream, _peer)) => {
                    if self.connections.len() >= self.config.max_connections {
                        drop(stream);
                        continue;
                    }

                    stream.set_nodelay(true)?;
                    let token = self.allocate_connection_token();
                    self.poll
                        .registry()
                        .register(&mut stream, token, Interest::READABLE)?;
                    self.connections.insert(
                        token,
                        CacheConnection::new(stream, self.config.max_pipeline_depth),
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(CacheServerError::Io(error)),
            }
        }
        Ok(())
    }

    fn handle_wake(&mut self) -> Result<(), CacheServerError> {
        self.install_published_placement();
        self.drain_control();
        self.inbox
            .drain(&mut self.store, self.config.max_inbox_batch);
        Ok(())
    }

    fn drain_control(&mut self) -> usize {
        let mut processed = 0;
        while processed < self.config.max_control_batch {
            let request = match self.control_rx.try_recv() {
                Ok(request) => request,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            self.handle_control(request);
            processed += 1;
        }

        // A bounded drain protects socket/inbox work. If control work remains,
        // re-wake this reactor so the next poll iteration continues promptly.
        if processed == self.config.max_control_batch {
            let _ = self.waker.wake();
        }
        processed
    }

    fn handle_control(&mut self, request: CacheShardControlRequest) {
        let now_ms = self.clock.now_ms();
        match request {
            CacheShardControlRequest::ExecuteRemoteCommand {
                placement_epoch,
                slot,
                frame,
                reply,
            } => {
                let _ =
                    reply.send(self.execute_remote_command(placement_epoch, slot, &frame, now_ms));
            }
            CacheShardControlRequest::ImportRemoteBatch {
                placement_epoch,
                batch,
                reply,
            } => {
                let _ = reply.send(self.import_remote_batch(placement_epoch, &batch, now_ms));
            }
            CacheShardControlRequest::Export {
                slot,
                cursor,
                max_entries,
                reply,
            } => {
                let batch = self
                    .store
                    .export_slot_batch(slot, cursor, max_entries, now_ms);
                let _ = reply.send(batch);
            }
            CacheShardControlRequest::Import { batch, reply } => {
                let elapsed_ms = now_ms.saturating_sub(batch.exported_at_ms);
                let tracker = self
                    .transfer_imports
                    .entry(batch.slot)
                    .or_insert_with(|| CacheTransferImportTracker::new(batch.slot));
                let results = batch
                    .entries
                    .iter()
                    .map(|entry| tracker.import_entry(&mut self.store, entry, elapsed_ms, now_ms))
                    .collect();
                let _ = reply.send(results);
            }
            CacheShardControlRequest::Finalize { entries, reply } => {
                let results = entries
                    .iter()
                    .map(|entry| self.store.finalize_transfer_entry(entry, now_ms))
                    .collect();
                let _ = reply.send(results);
            }
            CacheShardControlRequest::CountSlot { slot, reply } => {
                let _ = reply.send(self.store.live_entries_in_slot(slot, now_ms));
            }
            CacheShardControlRequest::ClearImportTracker { slot, reply } => {
                self.transfer_imports.remove(&slot);
                let _ = reply.send(());
            }
        }
    }

    fn execute_remote_command(
        &mut self,
        placement_epoch: u64,
        slot: u16,
        frame: &[u8],
        now_ms: u64,
    ) -> Result<Vec<u8>, CacheRemoteControlError> {
        let placement = self.dispatcher.placement();
        if placement.epoch() != placement_epoch {
            return Err(CacheRemoteControlError::TopologyChanged {
                installed_epoch: placement.epoch(),
                requested_epoch: placement_epoch,
            });
        }

        let local = CacheShardOwner {
            node_id: self.dispatcher.local_node_id(),
            shard: self.dispatcher.local_shard(),
        };
        if placement.owner_for_slot(slot) != Some(local) {
            return Err(CacheRemoteControlError::OwnerMismatch);
        }

        let Some((command, consumed)) =
            parse_command(frame).map_err(CacheRemoteControlError::Parse)?
        else {
            return Err(CacheRemoteControlError::InvalidFrame);
        };
        if consumed != frame.len() || command_slot(command) != RespCommandSlot::Slot(slot) {
            return Err(CacheRemoteControlError::InvalidFrame);
        }

        let mut out = Vec::with_capacity(128);
        execute_command(&mut self.store, command, now_ms, &mut out);
        Ok(out)
    }

    fn import_remote_batch(
        &mut self,
        placement_epoch: u64,
        batch: &CacheTransferBatch,
        now_ms: u64,
    ) -> Result<Vec<CacheTransferImport>, CacheRemoteControlError> {
        let placement = self.dispatcher.placement();
        if placement.epoch() != placement_epoch {
            return Err(CacheRemoteControlError::TopologyChanged {
                installed_epoch: placement.epoch(),
                requested_epoch: placement_epoch,
            });
        }

        let local = CacheShardOwner {
            node_id: self.dispatcher.local_node_id(),
            shard: self.dispatcher.local_shard(),
        };
        let Some(migration) = placement.migration_for_slot(batch.slot) else {
            return Err(CacheRemoteControlError::OwnerMismatch);
        };
        if migration.target != local {
            return Err(CacheRemoteControlError::OwnerMismatch);
        }

        let tracker = self
            .transfer_imports
            .entry(batch.slot)
            .or_insert_with(|| CacheTransferImportTracker::new(batch.slot));

        // Monotonic cache clocks are process-local. The source has already
        // reduced TTL for time spent before transport; cross-node wire time is
        // deliberately not inferred from unrelated clock origins.
        Ok(batch
            .entries
            .iter()
            .map(|entry| tracker.import_entry(&mut self.store, entry, 0, now_ms))
            .collect())
    }

    fn install_published_placement(&mut self) {
        let Some(publisher) = self.placement_publisher.as_ref() else {
            return;
        };
        let installed_epoch = self.dispatcher.placement().epoch();
        let Some(snapshot) = publisher.snapshot_if_newer(installed_epoch) else {
            return;
        };
        let epoch = snapshot.epoch();
        self.dispatcher.install_placement(snapshot);
        self.applied_placement_epoch.store(epoch, Ordering::Release);
    }

    fn connection_ready(
        &mut self,
        token: Token,
        event: ReadyEvent,
    ) -> Result<(), CacheServerError> {
        if event.error || event.read_closed || event.write_closed {
            self.close_connection(token);
            return Ok(());
        }
        self.drive_connection(token, event.readable, event.writable)
    }

    fn drive_connection(
        &mut self,
        token: Token,
        readable: bool,
        writable: bool,
    ) -> Result<(), CacheServerError> {
        let Some(mut connection) = self.connections.remove(&token) else {
            return Ok(());
        };

        let result = self.drive_connection_inner(&mut connection, readable, writable);
        match result {
            Ok(()) => {
                self.update_interest(token, &mut connection)?;
                self.connections.insert(token, connection);
            }
            Err(ConnectionAction::Close) => {
                let _ = self.poll.registry().deregister(&mut connection.stream);
            }
        }
        Ok(())
    }

    fn drive_connection_inner(
        &mut self,
        connection: &mut CacheConnection,
        readable: bool,
        writable: bool,
    ) -> Result<(), ConnectionAction> {
        if connection
            .pipeline
            .drain_ready(&mut connection.output)
            .is_err()
        {
            return Err(ConnectionAction::Close);
        }

        if readable && self.read_connection(connection).is_err() {
            return Err(ConnectionAction::Close);
        }

        if self.process_input(connection).is_err() {
            return Err(ConnectionAction::Close);
        }

        if connection.pending_output() > self.config.max_output_buffer {
            return Err(ConnectionAction::Close);
        }

        if (writable || connection.pending_output() != 0)
            && self.flush_connection(connection).is_err()
        {
            return Err(ConnectionAction::Close);
        }

        Ok(())
    }

    fn read_connection(&self, connection: &mut CacheConnection) -> io::Result<()> {
        connection.compact_input();
        let mut scratch = [0u8; 16 * 1024];

        loop {
            match connection.stream.read(&mut scratch) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "cache client closed connection",
                    ));
                }
                Ok(read) => {
                    if connection
                        .input
                        .len()
                        .saturating_sub(connection.input_start)
                        .saturating_add(read)
                        > self.config.max_input_buffer
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "cache input buffer limit exceeded",
                        ));
                    }
                    connection.input.extend_from_slice(&scratch[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn process_input(
        &mut self,
        connection: &mut CacheConnection,
    ) -> Result<(), CachePipelineError> {
        loop {
            if connection.input_start == connection.input.len() {
                connection.compact_input();
                return Ok(());
            }

            let input = &connection.input[connection.input_start..];
            let Some(submit) = connection.pipeline.submit_frame(
                &self.dispatcher,
                &mut self.store,
                input,
                self.clock.now_ms(),
                &mut connection.output,
            )?
            else {
                connection.compact_input();
                return Ok(());
            };

            if submit.remote.is_some() {
                return Err(CachePipelineError::Dispatch(
                    super::cache_dispatch::CacheDispatchError::QueueDisconnected(
                        self.dispatcher.local_shard(),
                    ),
                ));
            }

            connection.input_start += submit.consumed;
            if connection.pending_output() > self.config.max_output_buffer {
                connection.compact_input();
                return Ok(());
            }
        }
    }

    fn flush_connection(&self, connection: &mut CacheConnection) -> io::Result<()> {
        while connection.output_start < connection.output.len() {
            match connection
                .stream
                .write(&connection.output[connection.output_start..])
            {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "cache socket write returned zero",
                    ));
                }
                Ok(written) => connection.output_start += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        connection.compact_output();
        Ok(())
    }

    fn update_interest(
        &self,
        token: Token,
        connection: &mut CacheConnection,
    ) -> Result<(), CacheServerError> {
        let wants_writable = connection.pending_output() != 0;
        if wants_writable == connection.writable_interest {
            return Ok(());
        }

        let interest = if wants_writable {
            Interest::READABLE | Interest::WRITABLE
        } else {
            Interest::READABLE
        };
        self.poll
            .registry()
            .reregister(&mut connection.stream, token, interest)?;
        connection.writable_interest = wants_writable;
        Ok(())
    }

    fn close_connection(&mut self, token: Token) {
        if let Some(mut connection) = self.connections.remove(&token) {
            let _ = self.poll.registry().deregister(&mut connection.stream);
        }
    }

    fn allocate_connection_token(&mut self) -> Token {
        loop {
            let raw = self.next_connection_token;
            self.next_connection_token = self.next_connection_token.wrapping_add(1);
            if self.next_connection_token < FIRST_CONNECTION_TOKEN {
                self.next_connection_token = FIRST_CONNECTION_TOKEN;
            }

            let token = Token(raw);
            if token != LISTENER_TOKEN
                && token != WAKE_TOKEN
                && !self.connections.contains_key(&token)
            {
                return token;
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ConnectionAction {
    Close,
}

fn validate_placement_endpoints(
    placement: &CacheSlotMap,
    endpoints: &CacheEndpointMap,
) -> Result<(), CacheServiceError> {
    for range in placement.slot_ranges() {
        if endpoints.get(range.owner).is_none() {
            return Err(CacheServiceError::MissingEndpoint(range.owner));
        }
    }
    for (_, migration) in placement.migrations() {
        if endpoints.get(migration.target).is_none() {
            return Err(CacheServiceError::MissingEndpoint(migration.target));
        }
    }
    Ok(())
}

fn validate_config(config: &CacheServerConfig) -> Result<(), CacheServerError> {
    if config.max_connections == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_connections must be non-zero",
        ));
    }
    if config.max_input_buffer == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_input_buffer must be non-zero",
        ));
    }
    if config.max_output_buffer == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_output_buffer must be non-zero",
        ));
    }
    if config.max_pipeline_depth == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_pipeline_depth must be non-zero",
        ));
    }
    if config.events_capacity == 0 {
        return Err(CacheServerError::InvalidConfig(
            "events_capacity must be non-zero",
        ));
    }
    if config.max_inbox_batch == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_inbox_batch must be non-zero",
        ));
    }
    if config.control_queue_capacity == 0 {
        return Err(CacheServerError::InvalidConfig(
            "control_queue_capacity must be non-zero",
        ));
    }
    if config.max_control_batch == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_control_batch must be non-zero",
        ));
    }
    if config.expiry_sweep_interval.is_zero() {
        return Err(CacheServerError::InvalidConfig(
            "expiry_sweep_interval must be non-zero",
        ));
    }
    if config.max_expiry_items_per_sweep == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_expiry_items_per_sweep must be non-zero",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::cache_cluster::{CacheAdvertisedEndpoint, CacheEndpointMap};
    use super::super::cache_dispatch::CacheDispatchChannels;
    use super::super::cache_routing::CacheSlotMap;
    use super::*;
    use std::net::TcpStream as StdTcpStream;

    fn read_resp_line(client: &mut StdTcpStream) -> Vec<u8> {
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            client.read_exact(&mut byte).unwrap();
            response.push(byte[0]);
            if response.ends_with(b"\r\n") {
                return response;
            }
        }
    }

    fn wait_for_placement_epoch(handle: &CacheServiceHandle, epoch: u64) {
        for _ in 0..100 {
            if handle
                .shard_placement_epochs()
                .iter()
                .all(|applied| *applied >= epoch)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "cache shards did not apply epoch {epoch}; observed {:?}",
            handle.shard_placement_epochs()
        );
    }

    fn build_server() -> CacheShardServer {
        let placement = CacheSlotMap::new_local(1, 1).unwrap();
        let owner = placement.owner_for_slot(0).unwrap();
        let (channels, mut inboxes) = CacheDispatchChannels::new(1, 32).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(owner, CacheAdvertisedEndpoint::new("127.0.0.1", 7000));
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);

        CacheShardServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            dispatcher,
            inboxes.remove(0),
            CacheStore::new(),
            CacheServerConfig {
                expiry_sweep_interval: Duration::from_millis(2),
                ..CacheServerConfig::default()
            },
            CacheServerClock::new(),
        )
        .unwrap()
    }

    #[test]
    fn service_builder_publishes_real_ephemeral_ports() {
        let service = CacheServiceBuilder::local(11, 2)
            .unwrap()
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .build()
            .unwrap();

        assert_eq!(service.local_addrs().len(), 2);
        assert_ne!(service.local_addrs()[0].port(), 0);
        assert_ne!(service.local_addrs()[1].port(), 0);
        assert_ne!(service.local_addrs()[0], service.local_addrs()[1]);

        for shard in 0..2 {
            let owner = CacheShardOwner { node_id: 11, shard };
            let endpoint = service.endpoints().get(owner).unwrap();
            assert_eq!(
                endpoint.port(),
                service.local_addrs()[shard as usize].port()
            );
        }
    }

    #[test]
    fn service_builder_rejects_unadvertised_migration_target() {
        let mut placement = CacheSlotMap::new_local(31, 1).unwrap();
        let slot = 42;
        let source = placement.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 99,
            shard: 0,
        };
        placement.begin_migration(1, slot, source, target).unwrap();

        let result = CacheServiceBuilder::new(31, placement)
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .build();

        assert!(matches!(
            result,
            Err(CacheServiceError::MissingEndpoint(owner)) if owner == target
        ));
    }

    #[test]
    fn running_service_installs_migration_and_commit_epochs() {
        let placement = CacheSlotMap::new_local(41, 2).unwrap();
        let mut key = None;
        for index in 0..10_000 {
            let candidate = format!("live-topology-{index}").into_bytes();
            if placement.owner_for_key(&candidate).shard == 0 {
                key = Some(candidate);
                break;
            }
        }
        let key = key.expect("key for source shard");
        let slot = super::super::cache::redis_slot(&key);
        let source = placement.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 41,
            shard: 1,
        };

        let service = CacheServiceBuilder::new(41, placement.clone())
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .build()
            .unwrap();
        let handle = service.start().unwrap();
        let target_port = handle.local_addrs()[1].port();

        let mut migrating = placement;
        migrating.begin_migration(1, slot, source, target).unwrap();
        handle.install_placement(migrating.clone()).unwrap();
        wait_for_placement_epoch(&handle, 1);
        assert_eq!(handle.published_placement_epoch(), 1);

        let mut client = StdTcpStream::connect(handle.local_addrs()[0]).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = format!("*2\r\n$3\r\nGET\r\n${}\r\n", key.len()).into_bytes();
        request.extend_from_slice(&key);
        request.extend_from_slice(b"\r\n");
        client.write_all(&request).unwrap();

        let ask = read_resp_line(&mut client);
        let expected_ask = format!("-ASK {} 127.0.0.1:{target_port}\r\n", slot);
        assert_eq!(ask, expected_ask.as_bytes());

        migrating.commit_migration(2, slot, source, target).unwrap();
        handle.install_placement(migrating.clone()).unwrap();
        wait_for_placement_epoch(&handle, 2);

        client.write_all(&request).unwrap();
        let moved = read_resp_line(&mut client);
        let expected_moved = format!("-MOVED {} 127.0.0.1:{target_port}\r\n", slot);
        assert_eq!(moved, expected_moved.as_bytes());

        assert!(matches!(
            handle.install_placement(migrating),
            Err(CacheServiceError::StalePlacementEpoch {
                current: 2,
                proposed: 2,
            })
        ));

        handle.shutdown().unwrap();
    }

    #[test]
    fn local_transfer_coordinator_moves_key_between_running_shards() {
        let placement = CacheSlotMap::new_local(51, 2).unwrap();
        let mut key = None;
        for index in 0..10_000 {
            let candidate = format!("transfer-live-{index}").into_bytes();
            if placement.owner_for_key(&candidate).shard == 0 {
                key = Some(candidate);
                break;
            }
        }
        let key = key.expect("key for source shard");
        let slot = super::super::cache::redis_slot(&key);
        let source = placement.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 51,
            shard: 1,
        };

        let service = CacheServiceBuilder::new(51, placement.clone())
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .build()
            .unwrap();
        let handle = service.start().unwrap();
        let source_addr = handle.local_addrs()[0];
        let target_addr = handle.local_addrs()[1];

        // Seed the stable source through the real RESP listener.
        let mut source_client = StdTcpStream::connect(source_addr).unwrap();
        source_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut set = format!("*3\r\n$3\r\nSET\r\n${}\r\n", key.len()).into_bytes();
        set.extend_from_slice(&key);
        set.extend_from_slice(b"\r\n$5\r\nvalue\r\n");
        source_client.write_all(&set).unwrap();
        assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

        let mut migrating = placement;
        migrating.begin_migration(1, slot, source, target).unwrap();
        handle.install_placement(migrating.clone()).unwrap();
        wait_for_placement_epoch(&handle, 1);

        let report = handle
            .transfer_local_slot_batch(0, 1, slot, None, 8)
            .unwrap();
        assert_eq!(report.exported_entries, 1);
        assert_eq!(report.imported, 1);
        assert_eq!(report.finalized_removed, 1);
        assert_eq!(report.stale_source_versions, 0);
        assert_eq!(report.conflicts, 0);
        assert_eq!(report.source_remaining, 0);
        assert!(report.source_drained());
        assert!(!report.restart_scan_required());

        // The source no longer has the key, so migration routing now emits ASK.
        let mut get = format!("*2\r\n$3\r\nGET\r\n${}\r\n", key.len()).into_bytes();
        get.extend_from_slice(&key);
        get.extend_from_slice(b"\r\n");
        source_client.write_all(&get).unwrap();
        let expected_ask = format!("-ASK {} 127.0.0.1:{}\r\n", slot, target_addr.port());
        assert_eq!(read_resp_line(&mut source_client), expected_ask.as_bytes());

        // The importing target serves it only behind one-shot ASKING.
        let mut target_client = StdTcpStream::connect(target_addr).unwrap();
        target_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        target_client.write_all(b"*1\r\n$6\r\nASKING\r\n").unwrap();
        assert_eq!(read_resp_line(&mut target_client), b"+OK\r\n");
        target_client.write_all(&get).unwrap();
        let mut imported_value = [0u8; 11];
        target_client.read_exact(&mut imported_value).unwrap();
        assert_eq!(&imported_value, b"$5\r\nvalue\r\n");

        // Commit stable ownership and verify the target now serves normally.
        migrating.commit_migration(2, slot, source, target).unwrap();
        handle.install_placement(migrating).unwrap();
        wait_for_placement_epoch(&handle, 2);
        handle.clear_local_transfer_imports(1, slot).unwrap();

        target_client.write_all(&get).unwrap();
        let mut stable_value = [0u8; 11];
        target_client.read_exact(&mut stable_value).unwrap();
        assert_eq!(&stable_value, b"$5\r\nvalue\r\n");

        source_client.write_all(&get).unwrap();
        let expected_moved = format!("-MOVED {} 127.0.0.1:{}\r\n", slot, target_addr.port());
        assert_eq!(
            read_resp_line(&mut source_client),
            expected_moved.as_bytes()
        );

        handle.shutdown().unwrap();
    }

    #[test]
    fn service_redirects_to_the_reserved_peer_endpoint() {
        let placement = CacheSlotMap::new_local(23, 2).unwrap();
        let mut key = None;
        for index in 0..10_000 {
            let candidate = format!("peer-key-{index}").into_bytes();
            if placement.owner_for_key(&candidate).shard == 1 {
                key = Some(candidate);
                break;
            }
        }
        let key = key.expect("key for shard one");

        let service = CacheServiceBuilder::new(23, placement)
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .with_shard(CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            ))
            .build()
            .unwrap();
        let expected_port = service.local_addrs()[1].port();
        let handle = service.start().unwrap();

        let mut client = StdTcpStream::connect(handle.local_addrs()[0]).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let mut request = format!("*2\r\n$3\r\nGET\r\n${}\r\n", key.len()).into_bytes();
        request.extend_from_slice(&key);
        request.extend_from_slice(b"\r\n");
        client.write_all(&request).unwrap();

        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            client.read_exact(&mut byte).unwrap();
            response.push(byte[0]);
            if response.ends_with(b"\r\n") {
                break;
            }
        }

        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("-MOVED "));
        assert!(response.ends_with(&format!(" 127.0.0.1:{expected_port}\r\n")));

        handle.shutdown().unwrap();
    }

    #[test]
    fn reactor_serves_pipelined_set_and_get() {
        let mut server = build_server();
        let address = server.local_addr().unwrap();
        let mut client = StdTcpStream::connect(address).unwrap();
        client.set_nodelay(true).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let request = [
            b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n".as_slice(),
            b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n".as_slice(),
        ]
        .concat();
        client.write_all(&request).unwrap();

        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(10))).unwrap();
        }

        let mut response = [0u8; 16];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"+OK\r\n$5\r\nvalue\r\n");
    }

    #[test]
    fn shutdown_control_wakes_blocked_reactor() {
        let mut server = build_server();
        let control = server.control();

        let thread = std::thread::spawn(move || server.run());
        std::thread::sleep(Duration::from_millis(10));
        control.shutdown();

        thread.join().unwrap().unwrap();
    }

    #[test]
    fn idle_reactor_purges_expired_values() {
        let mut server = build_server();
        let now = server.clock.now_ms();
        server.store_mut().set_bytes(b"ttl", b"value", Some(1), now);

        std::thread::sleep(Duration::from_millis(3));
        server.poll_once(Some(Duration::from_millis(1))).unwrap();

        let now = server.clock.now_ms();
        assert!(server.store_mut().get(b"ttl", now).is_none());
    }

    #[test]
    fn redirect_mode_is_required() {
        let placement = CacheSlotMap::new_local(1, 1).unwrap();
        let (channels, mut inboxes) = CacheDispatchChannels::new(1, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels).unwrap();

        let result = CacheShardServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            dispatcher,
            inboxes.remove(0),
            CacheStore::new(),
            CacheServerConfig::default(),
            CacheServerClock::new(),
        );

        assert!(matches!(
            result,
            Err(CacheServerError::RedirectModeRequired)
        ));
    }
}
