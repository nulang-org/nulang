//! Dedicated readiness reactor for the RESP cache tier.
//!
//! The cache server is intentionally separate from the actor scheduler. Each
//! physical cache shard owns its listener, connections, CacheStore, and expiry
//! work on one thread. Cluster-aware clients are redirected to the owning shard
//! before execution, so the steady-state GET/SET path stays thread-local.

#![cfg(feature = "cache-server")]

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};

use super::cache::CacheStore;
use super::cache_cluster::CacheRoutingMode;
use super::cache_dispatch::{
    CacheDispatchConfigError, CacheDispatchOutcome, CacheDispatchWake, CacheDispatcher,
    CacheShardInbox,
};
use super::resp::parse_command;
#[cfg(unix)]
use super::{
    cache::CacheLimits,
    cache_durable::{CacheTime, DurableCache, DurableError},
    cache_wal::{WalLimits, WalSync},
};
#[cfg(unix)]
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const LISTENER_TOKEN: Token = Token(0);
const WAKE_TOKEN: Token = Token(1);
const FIRST_CONNECTION_TOKEN: usize = 2;
const ACCEPT_QUANTUM: usize = 32;
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const COMMAND_QUANTUM: usize = 32;
const READ_QUANTUM: usize = 32;
const WRITE_QUANTUM: usize = 32;
const IO_CHUNK_BYTES: usize = 16 * 1024;

/// Shared admission budget for retained RESP vector capacity and TLS reservations.
/// Clone this budget into multiple shard reactors to share one process allowance.
#[derive(Debug, Clone)]
pub struct CacheBufferBudget {
    inner: Arc<BufferBudgetInner>,
}
#[derive(Debug)]
struct BufferBudgetInner {
    limit: usize,
    used: AtomicUsize,
    peers: Mutex<HashMap<IpAddr, usize>>,
}
impl CacheBufferBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(BufferBudgetInner {
                limit,
                used: AtomicUsize::new(0),
                peers: Mutex::new(HashMap::new()),
            }),
        }
    }
    /// Configured shared admission limit.
    pub fn limit_bytes(&self) -> usize {
        self.inner.limit
    }

    /// Total charged vector capacities plus fixed TLS admission reservations.
    pub fn used_bytes(&self) -> usize {
        self.inner.used.load(Ordering::Acquire)
    }
    fn acquire(&self, amount: usize) -> bool {
        self.inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(amount)
                    .filter(|next| *next <= self.inner.limit)
            })
            .is_ok()
    }
    fn admit_peer(&self, address: IpAddr, limit: usize) -> Option<PeerLease> {
        if limit == 0 {
            return None;
        }
        let address = address.to_canonical();
        let mut peers = self.inner.peers.lock().ok()?;
        let count = peers.entry(address).or_default();
        if *count >= limit {
            return None;
        }
        *count += 1;
        Some(PeerLease {
            budget: self.clone(),
            address,
        })
    }

    fn lease(&self) -> BufferLease {
        BufferLease {
            budget: self.clone(),
            charged: 0,
        }
    }
}
struct PeerLease {
    budget: CacheBufferBudget,
    address: IpAddr,
}

impl Drop for PeerLease {
    fn drop(&mut self) {
        let mut peers = self
            .budget
            .inner
            .peers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = peers.get_mut(&self.address) {
            *count -= 1;
            if *count == 0 {
                peers.remove(&self.address);
            }
        }
    }
}

struct BufferLease {
    budget: CacheBufferBudget,
    charged: usize,
}
impl BufferLease {
    fn reserve_tls(&mut self) -> bool {
        if !self.budget.acquire(TLS_RESERVATION_BYTES) {
            return false;
        }
        self.charged += TLS_RESERVATION_BYTES;
        true
    }

    fn reserve(&mut self, buffer: &mut Vec<u8>, required: usize, max: usize) -> io::Result<()> {
        let denied = || {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "cache connection buffer budget exceeded",
            )
        };
        if required > max {
            return Err(denied());
        }
        let old = buffer.capacity();
        if required <= old {
            return Ok(());
        }
        let preferred = required.max(old.saturating_mul(2)).max(4096).min(max);
        let target = if self.budget.acquire(preferred - old) {
            preferred
        } else if self.budget.acquire(required - old) {
            required
        } else {
            return Err(denied());
        };
        self.charged += target - old;
        if buffer.try_reserve_exact(target - buffer.len()).is_err() {
            return Err(denied());
        }
        // Vec may report more capacity than requested. Fail closed if the
        // allocator's additional capacity cannot be charged.
        let extra = buffer.capacity().saturating_sub(target);
        if extra != 0 {
            if !self.budget.acquire(extra) {
                return Err(denied());
            }
            self.charged += extra;
        }
        Ok(())
    }

    /// Release a drained vector's allocation before returning its charge.
    /// Never discard bytes still needed by the parser or socket writer.
    fn release_empty(&mut self, buffer: &mut Vec<u8>) {
        if !buffer.is_empty() {
            return;
        }
        let capacity = buffer.capacity();
        if capacity == 0 {
            return;
        }
        drop(std::mem::take(buffer));
        self.charged -= capacity;
        self.budget.inner.used.fetch_sub(capacity, Ordering::AcqRel);
    }
}
impl Drop for BufferLease {
    fn drop(&mut self) {
        self.budget
            .inner
            .used
            .fetch_sub(self.charged, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone)]
pub struct CacheServerConfig {
    pub max_connections: usize,
    /// Concurrent connections from one canonical source IP across shared-budget shards.
    pub max_connections_per_peer: usize,
    pub max_total_connection_bytes: usize,
    pub max_input_buffer: usize,
    pub max_output_buffer: usize,
    pub max_pipeline_depth: usize,
    pub events_capacity: usize,
    pub max_inbox_batch: usize,
    pub expiry_sweep_interval: Duration,
    pub max_expiry_items_per_sweep: usize,
}

impl Default for CacheServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 16_384,
            max_connections_per_peer: 64,
            max_total_connection_bytes: 256 * 1024 * 1024,
            max_input_buffer: 1024 * 1024,
            max_output_buffer: 8 * 1024 * 1024,
            max_pipeline_depth: 1024,
            events_capacity: 1024,
            max_inbox_batch: 256,
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
    #[cfg(unix)]
    Durability(DurableError),
    #[cfg(unix)]
    DurableShardPoisoned,
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

#[derive(Clone)]
pub struct CacheShardServerControl {
    shutdown: Arc<AtomicBool>,
    waker: Arc<Waker>,
}

impl CacheShardServerControl {
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.waker.wake();
    }

    pub fn wake(&self) {
        let _ = self.waker.wake();
    }
}

impl CacheDispatchWake for Waker {
    fn wake(&self) {
        let _ = Waker::wake(self);
    }
}

/// Explicit client-certificate authentication using a private trust root.
#[derive(Clone)]
pub struct CacheMutualTls {
    config: Arc<rustls::ServerConfig>,
}

impl CacheMutualTls {
    pub fn from_pem(mut ca: &[u8], mut cert: &[u8], mut key: &[u8]) -> io::Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in rustls_pemfile::certs(&mut ca) {
            roots.add(certificate?).map_err(io::Error::other)?;
        }
        if roots.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty client CA",
            ));
        }
        let certificates = rustls_pemfile::certs(&mut cert).collect::<Result<Vec<_>, _>>()?;
        let key = rustls_pemfile::private_key(&mut key)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing server private key")
        })?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .map_err(io::Error::other)?;
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(certificates, key)
            .map_err(io::Error::other)?;
        config.max_early_data_size = 0;
        config.send_tls13_tickets = 0;
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        Ok(Self {
            config: Arc::new(config),
        })
    }
}

const TLS_BUFFER_LIMIT: usize = 64 * 1024;
/// Conservative admission charge, not a measured or guaranteed RSS bound.
/// Covers modeled record/handshake buffers, session state and headroom.
pub const TLS_RESERVATION_BYTES: usize = 512 * 1024;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const TLS_CONNECTION_LIMIT: usize = 256;

struct CacheConnection {
    stream: TcpStream,
    input: Vec<u8>,
    input_start: usize,
    output: Vec<u8>,
    output_start: usize,
    writable_interest: bool,
    tls: Option<Box<rustls::ServerConnection>>,
    lease: BufferLease,
    _peer: PeerLease,
    accepted_at: Instant,
    commands_remaining: usize,
}

impl CacheConnection {
    fn new(stream: TcpStream, lease: BufferLease, peer: PeerLease) -> Self {
        Self {
            stream,
            input: Vec::new(),
            input_start: 0,
            output: Vec::new(),
            output_start: 0,
            lease,
            _peer: peer,
            writable_interest: false,
            tls: None,
            accepted_at: Instant::now(),
            commands_remaining: 0,
        }
    }

    fn pending_output(&self) -> usize {
        self.output.len().saturating_sub(self.output_start)
    }

    fn has_pending_wire(&self) -> bool {
        self.pending_output() != 0 || self.tls.as_ref().is_some_and(|tls| tls.wants_write())
    }

    fn handshake_pending(&self) -> bool {
        self.tls.as_ref().is_some_and(|tls| tls.is_handshaking())
    }

    // Drain plaintext before reading more encrypted records. The outer
    // reactor retains its existing application-buffer admission policy.
    // None means the quantum ran out before reaching WouldBlock.
    fn read_plaintext(
        &mut self,
        scratch: &mut [u8],
        remaining: &mut usize,
    ) -> io::Result<Option<usize>> {
        let Some(tls) = &mut self.tls else {
            if *remaining == 0 {
                return Ok(None);
            }
            *remaining -= 1;
            return self.stream.read(scratch).map(Some);
        };
        loop {
            if *remaining == 0 {
                return Ok(None);
            }
            if tls.is_handshaking() && self.accepted_at.elapsed() >= TLS_HANDSHAKE_TIMEOUT {
                return Err(io::ErrorKind::TimedOut.into());
            }
            if !tls.is_handshaking() {
                *remaining -= 1;
                match tls.reader().read(scratch) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    result => return result.map(Some),
                }
            }
            if *remaining == 0 {
                return Ok(None);
            }
            *remaining -= 1;
            let n = tls.read_tls(&mut self.stream)?;
            if n == 0 {
                return Ok(Some(0));
            }
            tls.process_new_packets().map_err(io::Error::other)?;
        }
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

impl ReadyEvent {
    fn continuation(token: Token, readable: bool, writable: bool) -> Self {
        Self {
            token,
            readable,
            writable,
            error: false,
            read_closed: false,
            write_closed: false,
        }
    }
    fn merge(&mut self, other: Self) {
        self.readable |= other.readable;
        self.writable |= other.writable;
        self.error |= other.error;
        self.read_closed |= other.read_closed;
        self.write_closed |= other.write_closed;
    }
}

/// Explicit durable mode configuration. Memory mode remains the default bind API.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct CacheDurableConfig {
    pub path: PathBuf,
    pub cache_limits: CacheLimits,
    pub wal_limits: WalLimits,
    pub sync: WalSync,
}

/// Automatic maintenance is opt-in for embedded reactors.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
pub struct AutomaticCheckpointConfig {
    pub trigger_percent: u8,
    pub min_savings_percent: u8,
    pub retry_cooldown_ms: u64,
}

#[cfg(unix)]
impl Default for AutomaticCheckpointConfig {
    fn default() -> Self {
        Self {
            trigger_percent: 70,
            min_savings_percent: 10,
            retry_cooldown_ms: 5000,
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AutomaticCheckpointStats {
    pub completed: u64,
    pub skipped_ineffective: u64,
    pub failed: u64,
    pub reclaimed_bytes: u64,
}

#[cfg(unix)]
struct AutomaticCheckpoint {
    config: AutomaticCheckpointConfig,
    next_attempt_ms: u64,
    stats: AutomaticCheckpointStats,
}

enum CacheBackend {
    Memory(CacheStore),
    #[cfg(unix)]
    Durable {
        cache: DurableCache,
        anchor: CacheTime,
    },
}

#[cfg(unix)]
fn durable_clock(anchor: CacheTime, now: u64) -> Result<CacheTime, DurableError> {
    let elapsed = now
        .checked_sub(anchor.monotonic_ms)
        .ok_or(DurableError::InvalidTime)?;
    Ok(CacheTime {
        monotonic_ms: now,
        unix_ms: anchor
            .unix_ms
            .checked_add(elapsed)
            .ok_or(DurableError::InvalidTime)?,
    })
}

/// Shard-local counters; command counts include dispatched error/redirect
/// replies, not a guarantee that a mutation committed or a reply arrived.
#[derive(Debug, Default, Clone, Copy)]
pub struct CacheOperationalCounters {
    pub accepted: u64,
    pub rejected_limit: u64,
    pub rejected_peer: u64,
    pub rejected_tls_budget: u64,
    pub accept_resource_errors: u64,
    pub connection_errors: u64,
    pub handshake_timeouts: u64,
    pub commands_dispatched: u64,
    pub polls_completed: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CacheOperationalSnapshot {
    pub healthy: bool,
    pub ready: bool,
    pub connections: usize,
    pub tls_reserved_bytes: usize,
    pub connection_budget_used_bytes: usize,
    pub connection_budget_limit_bytes: usize,
    pub cache: super::cache::CacheMemoryStats,
    pub cache_stats: super::cache::CacheStats,
    pub counters: CacheOperationalCounters,
    pub durable: bool,
    pub journal_bytes: u64,
    pub journal_limit_bytes: u64,
    pub checkpoint_completed: u64,
    pub checkpoint_failed: u64,
    pub checkpoint_skipped: u64,
    pub checkpoint_reclaimed_bytes: u64,
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
    deferred: HashMap<Token, ReadyEvent>,
    listener: Option<TcpListener>,
    accept_retry_at: Option<Instant>,
    dispatcher: CacheDispatcher,
    inbox: CacheShardInbox,
    backend: CacheBackend,
    connections: HashMap<Token, CacheConnection>,
    next_connection_token: usize,
    config: CacheServerConfig,
    buffer_budget: CacheBufferBudget,
    clock: CacheServerClock,
    shutdown: Arc<AtomicBool>,
    waker: Arc<Waker>,
    next_expiry_sweep: Instant,
    mutual_tls: Option<CacheMutualTls>,
    started: bool,
    counters: CacheOperationalCounters,
    #[cfg(unix)]
    automatic_checkpoint: Option<AutomaticCheckpoint>,
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
        let budget = CacheBufferBudget::new(config.max_total_connection_bytes);
        Self::bind_with_budget(bind_addr, dispatcher, inbox, store, config, clock, budget)
    }

    /// Bind a shard using a budget shared by all participating reactors.
    pub fn bind_with_budget(
        bind_addr: SocketAddr,
        dispatcher: CacheDispatcher,
        inbox: CacheShardInbox,
        store: CacheStore,
        config: CacheServerConfig,
        clock: CacheServerClock,
        buffer_budget: CacheBufferBudget,
    ) -> Result<Self, CacheServerError> {
        Self::bind_backend(
            bind_addr,
            dispatcher,
            inbox,
            CacheBackend::Memory(store),
            config,
            clock,
            buffer_budget,
        )
    }

    /// Recover a private journal before accepting any network traffic.
    #[cfg(unix)]
    pub fn bind_durable(
        bind_addr: SocketAddr,
        dispatcher: CacheDispatcher,
        inbox: CacheShardInbox,
        durability: CacheDurableConfig,
        config: CacheServerConfig,
        clock: CacheServerClock,
    ) -> Result<Self, CacheServerError> {
        let budget = CacheBufferBudget::new(config.max_total_connection_bytes);
        Self::bind_durable_with_budget(
            bind_addr, dispatcher, inbox, durability, config, clock, budget,
        )
    }

    #[cfg(unix)]
    pub fn bind_durable_with_budget(
        bind_addr: SocketAddr,
        dispatcher: CacheDispatcher,
        inbox: CacheShardInbox,
        durability: CacheDurableConfig,
        config: CacheServerConfig,
        clock: CacheServerClock,
        buffer_budget: CacheBufferBudget,
    ) -> Result<Self, CacheServerError> {
        validate_config(&config)?;
        if dispatcher.routing_mode() != CacheRoutingMode::Redirect {
            return Err(CacheServerError::RedirectModeRequired);
        }
        let anchor = CacheTime {
            monotonic_ms: clock.now_ms(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|duration| u64::try_from(duration.as_millis()).ok())
                .ok_or(CacheServerError::Durability(DurableError::InvalidTime))?,
        };
        let cache = DurableCache::open(
            durability.path,
            durability.cache_limits,
            durability.wal_limits,
            durability.sync,
            anchor,
        )
        .map_err(CacheServerError::Durability)?;
        Self::bind_backend(
            bind_addr,
            dispatcher,
            inbox,
            CacheBackend::Durable { cache, anchor },
            config,
            clock,
            buffer_budget,
        )
    }

    fn bind_backend(
        bind_addr: SocketAddr,
        dispatcher: CacheDispatcher,
        inbox: CacheShardInbox,
        backend: CacheBackend,
        config: CacheServerConfig,
        clock: CacheServerClock,
        buffer_budget: CacheBufferBudget,
    ) -> Result<Self, CacheServerError> {
        validate_config(&config)?;
        if buffer_budget.inner.limit == 0 {
            return Err(CacheServerError::InvalidConfig(
                "shared connection byte budget must be non-zero",
            ));
        }
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
        let mut listener = TcpListener::bind(bind_addr)?;
        poll.registry()
            .register(&mut listener, LISTENER_TOKEN, Interest::READABLE)?;

        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN)?);
        dispatcher.install_waker(inbox.shard(), waker.clone())?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let next_expiry_sweep = Instant::now() + config.expiry_sweep_interval;

        Ok(Self {
            events: Events::with_capacity(config.events_capacity),
            ready: Vec::with_capacity(config.events_capacity),
            deferred: HashMap::new(),
            poll,
            listener: Some(listener),
            accept_retry_at: None,
            dispatcher,
            inbox,
            backend,
            connections: HashMap::new(),
            next_connection_token: FIRST_CONNECTION_TOKEN,
            config,
            buffer_budget,
            clock,
            shutdown,
            waker,
            next_expiry_sweep,
            mutual_tls: None,
            started: false,
            counters: CacheOperationalCounters::default(),
            #[cfg(unix)]
            automatic_checkpoint: None,
        })
    }

    /// Configure mutual TLS before the first reactor iteration. No plaintext
    /// fallback is permitted. Limit TLS connection count independently of
    /// RESP vector budgets because rustls owns additional allocations.
    pub fn enable_mutual_tls(&mut self, tls: CacheMutualTls) -> Result<(), CacheServerError> {
        if self.started || self.mutual_tls.is_some() {
            return Err(CacheServerError::InvalidConfig(
                "TLS must be configured before polling",
            ));
        }
        if self.buffer_budget.inner.limit <= TLS_RESERVATION_BYTES {
            return Err(CacheServerError::InvalidConfig(
                "shared connection budget must exceed one TLS reservation",
            ));
        }
        self.mutual_tls = Some(tls);
        Ok(())
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "cache listener closed"))?
            .local_addr()
    }

    pub fn control(&self) -> CacheShardServerControl {
        CacheShardServerControl {
            shutdown: self.shutdown.clone(),
            waker: self.waker.clone(),
        }
    }

    /// Modeled TLS reservation held by this shard, excluding RESP vectors.
    pub fn tls_reserved_bytes(&self) -> usize {
        self.connections
            .values()
            .filter(|connection| connection.tls.is_some())
            .count()
            * TLS_RESERVATION_BYTES
    }

    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Read only on the owning reactor. Shared connection budget gauges
    /// are process-wide when multiple shards share that budget.
    pub fn operational_snapshot(&self) -> CacheOperationalSnapshot {
        let (store, durable, poisoned, journal_bytes, journal_limit_bytes) = match &self.backend {
            CacheBackend::Memory(store) => (store, false, false, 0, 0),
            #[cfg(unix)]
            CacheBackend::Durable { cache, .. } => {
                let (used, limit) = cache.journal_usage();
                (
                    cache.operational_store(),
                    true,
                    cache.is_poisoned(),
                    used,
                    limit,
                )
            }
        };
        #[cfg(unix)]
        let (completed, failed, skipped, reclaimed) =
            self.automatic_checkpoint_stats().map_or((0, 0, 0, 0), |s| {
                (
                    s.completed,
                    s.failed,
                    s.skipped_ineffective,
                    s.reclaimed_bytes,
                )
            });
        #[cfg(not(unix))]
        let (completed, failed, skipped, reclaimed) = (0, 0, 0, 0);
        let healthy =
            !poisoned && self.listener.is_some() && !self.shutdown.load(Ordering::Acquire);
        CacheOperationalSnapshot {
            healthy,
            ready: healthy && self.accept_retry_at.is_none(),
            connections: self.connection_count(),
            tls_reserved_bytes: self.tls_reserved_bytes(),
            connection_budget_used_bytes: self.buffer_budget.used_bytes(),
            connection_budget_limit_bytes: self.buffer_budget.limit_bytes(),
            cache: store.memory_stats(),
            cache_stats: store.stats(),
            counters: self.counters,
            durable,
            journal_bytes,
            journal_limit_bytes,
            checkpoint_completed: completed,
            checkpoint_failed: failed,
            checkpoint_skipped: skipped,
            checkpoint_reclaimed_bytes: reclaimed,
        }
    }

    /// Direct native access is memory-mode only; durable mutations must be journaled.
    pub fn store(&self) -> Option<&CacheStore> {
        match &self.backend {
            CacheBackend::Memory(store) => Some(store),
            #[cfg(unix)]
            CacheBackend::Durable { .. } => None,
        }
    }

    pub fn store_mut(&mut self) -> Option<&mut CacheStore> {
        match &mut self.backend {
            CacheBackend::Memory(store) => Some(store),
            #[cfg(unix)]
            CacheBackend::Durable { .. } => None,
        }
    }

    /// Configure an optional durable clock reservation before polling.
    #[cfg(unix)]
    pub fn configure_clock_reservation(&mut self, millis: u64) -> Result<(), CacheServerError> {
        if self.started {
            return Err(CacheServerError::InvalidConfig(
                "clock reservation must be configured before polling",
            ));
        }
        match &mut self.backend {
            CacheBackend::Memory(_) => Err(CacheServerError::InvalidConfig(
                "clock reservation requires durable mode",
            )),
            CacheBackend::Durable { cache, .. } => cache
                .configure_clock_reservation(millis)
                .map_err(CacheServerError::Durability),
        }
    }

    /// Explicit maintenance checkpoint on the owning reactor thread.
    /// The reactor does not serve commands while the replacement is written.
    #[cfg(unix)]
    pub fn checkpoint(&mut self) -> Result<super::cache_wal::CheckpointReport, CacheServerError> {
        self.ensure_healthy()?;
        let now = self.clock.now_ms();
        let result = match &mut self.backend {
            CacheBackend::Memory(_) => {
                return Err(CacheServerError::InvalidConfig(
                    "checkpoint requires durable mode",
                ))
            }
            CacheBackend::Durable { cache, anchor } => {
                let time = durable_clock(*anchor, now).map_err(CacheServerError::Durability)?;
                cache.checkpoint(time).map_err(CacheServerError::Durability)
            }
        };
        // Ambiguous publication must stop the entire shard, not just writes.
        self.ensure_healthy()?;
        result
    }

    /// Enable synchronous checkpoint maintenance for a durable shard.
    #[cfg(unix)]
    pub fn enable_automatic_checkpoint(
        &mut self,
        config: AutomaticCheckpointConfig,
    ) -> Result<(), CacheServerError> {
        if !(1..=99).contains(&config.trigger_percent)
            || !(1..=99).contains(&config.min_savings_percent)
            || config.retry_cooldown_ms == 0
        {
            return Err(CacheServerError::InvalidConfig(
                "invalid automatic checkpoint policy",
            ));
        }
        if !matches!(&self.backend, CacheBackend::Durable { .. }) {
            return Err(CacheServerError::InvalidConfig(
                "automatic checkpoint requires durable mode",
            ));
        }
        self.automatic_checkpoint = Some(AutomaticCheckpoint {
            config,
            next_attempt_ms: 0,
            stats: AutomaticCheckpointStats::default(),
        });
        Ok(())
    }

    #[cfg(unix)]
    pub fn automatic_checkpoint_stats(&self) -> Option<AutomaticCheckpointStats> {
        self.automatic_checkpoint
            .as_ref()
            .map(|policy| policy.stats)
    }

    #[cfg(unix)]
    fn maybe_checkpoint(&mut self, now_ms: u64) -> Result<(), CacheServerError> {
        let Some(policy) = &mut self.automatic_checkpoint else {
            return Ok(());
        };
        if now_ms < policy.next_attempt_ms {
            return Ok(());
        }
        let CacheBackend::Durable { cache, anchor } = &mut self.backend else {
            return Ok(());
        };
        let (used, limit) = cache.journal_usage();
        if u128::from(used) * 100 < u128::from(limit) * u128::from(policy.config.trigger_percent) {
            return Ok(());
        }
        let compacted = cache.estimated_checkpoint_bytes(now_ms);
        let savings = used.saturating_sub(compacted);
        if compacted >= used
            || u128::from(savings) * 100
                < u128::from(used) * u128::from(policy.config.min_savings_percent)
        {
            policy.stats.skipped_ineffective = policy.stats.skipped_ineffective.saturating_add(1);
            policy.next_attempt_ms = now_ms.saturating_add(policy.config.retry_cooldown_ms);
            return Ok(());
        }
        let time = durable_clock(*anchor, now_ms).map_err(CacheServerError::Durability)?;
        match cache.checkpoint(time) {
            Ok(report) => {
                policy.stats.completed = policy.stats.completed.saturating_add(1);
                policy.stats.reclaimed_bytes = policy
                    .stats
                    .reclaimed_bytes
                    .saturating_add(report.before_bytes.saturating_sub(report.after_bytes));
            }
            Err(_) => {
                policy.stats.failed = policy.stats.failed.saturating_add(1);
                policy.next_attempt_ms = now_ms.saturating_add(policy.config.retry_cooldown_ms);
            }
        }
        // Non-poisoning maintenance failures retain the old journal and retry
        // after a cooldown. Ambiguous publication is always fail-stop.
        self.ensure_healthy()
    }

    fn ensure_healthy(&mut self) -> Result<(), CacheServerError> {
        #[cfg(unix)]
        if matches!(&self.backend, CacheBackend::Durable { cache, .. } if cache.is_poisoned()) {
            for (_, mut connection) in self.connections.drain() {
                let _ = self.poll.registry().deregister(&mut connection.stream);
            }
            self.deferred.clear();
            self.close_listener();
            self.shutdown.store(true, Ordering::Release);
            return Err(CacheServerError::DurableShardPoisoned);
        }
        Ok(())
    }

    pub fn run(&mut self) -> Result<(), CacheServerError> {
        self.run_with_shutdown_timeout(Duration::from_secs(5))
    }

    /// Stop accepting/reading on shutdown, drain already encoded replies
    /// until the deadline, then synchronize the durable journal. No new
    /// commands execute during the drain, even on existing connections.
    pub fn run_with_shutdown_timeout(&mut self, timeout: Duration) -> Result<(), CacheServerError> {
        self.run_loop(timeout, None)
    }

    /// Publish periodic snapshots without taking a lock on every command.
    /// The observer must be nonblocking and must not run untrusted code.
    pub fn run_observed(
        &mut self,
        timeout: Duration,
        mut observer: impl FnMut(CacheOperationalSnapshot),
    ) -> Result<(), CacheServerError> {
        self.run_loop(timeout, Some(&mut observer))
    }

    fn run_loop(
        &mut self,
        timeout: Duration,
        mut observer: Option<&mut dyn FnMut(CacheOperationalSnapshot)>,
    ) -> Result<(), CacheServerError> {
        const INTERVAL: Duration = Duration::from_millis(250);
        let mut next = Instant::now();
        let result = (|| {
            self.ensure_healthy()?;
            while !self.shutdown.load(Ordering::Acquire) {
                if let Some(publish) = observer.as_deref_mut() {
                    if Instant::now() >= next {
                        publish(self.operational_snapshot());
                        next = Instant::now() + INTERVAL;
                    }
                }
                let wait = observer
                    .as_ref()
                    .map(|_| next.saturating_duration_since(Instant::now()));
                match self.poll_once(wait) {
                    Err(CacheServerError::Io(error))
                        if error.kind() == io::ErrorKind::Interrupted =>
                    {
                        continue
                    }
                    other => other?,
                }
            }
            if let Some(publish) = observer.as_deref_mut() {
                publish(self.operational_snapshot());
            }
            self.finish_shutdown(timeout)
        })();
        if let Some(publish) = observer {
            let mut snapshot = self.operational_snapshot();
            snapshot.healthy = false;
            snapshot.ready = false;
            publish(snapshot);
        }
        result
    }

    fn close_listener(&mut self) {
        self.accept_retry_at = None;
        if let Some(mut listener) = self.listener.take() {
            let _ = self.poll.registry().deregister(&mut listener);
        }
    }

    fn finish_shutdown(&mut self, timeout: Duration) -> Result<(), CacheServerError> {
        self.ensure_healthy()?;
        self.close_listener();
        self.deferred.clear();
        let start = Instant::now();
        // Attempt a nonblocking flush only for replies already encoded.
        // Drop incomplete/unexecuted input and idle clients immediately.
        let mut connections = std::mem::take(&mut self.connections);
        for (token, mut connection) in connections.drain() {
            connection.input.clear();
            connection.input_start = 0;
            connection.lease.release_empty(&mut connection.input);
            if start.elapsed() < timeout
                && connection.has_pending_wire()
                && Self::flush_connection_while(&mut connection, || start.elapsed() < timeout)
                    .is_ok()
                && connection.has_pending_wire()
                && self
                    .poll
                    .registry()
                    .reregister(&mut connection.stream, token, Interest::WRITABLE)
                    .is_ok()
            {
                self.connections.insert(token, connection);
            } else {
                let _ = self.poll.registry().deregister(&mut connection.stream);
            }
        }
        let drain_result = (|| -> io::Result<()> {
            while !self.connections.is_empty() {
                let remaining = timeout.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    break;
                }
                match self.poll.poll(&mut self.events, Some(remaining)) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    other => other?,
                }
                for event in self.events.iter() {
                    if start.elapsed() >= timeout {
                        break;
                    }
                    let Some(connection) = self.connections.get_mut(&event.token()) else {
                        continue;
                    };
                    if event.is_error()
                        || event.is_write_closed()
                        || (event.is_writable()
                            && (Self::flush_connection_while(connection, || {
                                start.elapsed() < timeout
                            })
                            .is_err()
                                || !connection.has_pending_wire()))
                    {
                        let mut connection = self
                            .connections
                            .remove(&event.token())
                            .expect("connection present");
                        let _ = self.poll.registry().deregister(&mut connection.stream);
                    }
                }
            }
            Ok(())
        })();
        for (_, mut connection) in self.connections.drain() {
            let _ = self.poll.registry().deregister(&mut connection.stream);
        }
        // Always try final sync, even if the network drain encountered an
        // error. A sync failure is never reported as a clean shutdown.
        #[cfg(unix)]
        if let CacheBackend::Durable { cache, .. } = &mut self.backend {
            cache
                .synchronize_at(self.clock.now_ms())
                .map_err(CacheServerError::Durability)?;
        }
        drain_result?;
        Ok(())
    }

    /// Drive one reactor iteration. A caller-supplied timeout is capped by the
    /// next expiration sweep deadline so TTL reclamation progresses while idle.
    pub fn poll_once(&mut self, timeout: Option<Duration>) -> Result<(), CacheServerError> {
        self.ensure_healthy()?;
        self.started = true;
        if self.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        #[cfg(unix)]
        self.maybe_checkpoint(self.clock.now_ms())?;
        let timeout = self.poll_timeout(timeout);
        self.poll.poll(&mut self.events, timeout)?;

        // A slow unauthenticated handshake cannot retain a slot forever.
        if self.mutual_tls.is_some() {
            let expired: Vec<_> = self
                .connections
                .iter()
                .filter(|(_, c)| {
                    c.handshake_pending() && c.accepted_at.elapsed() >= TLS_HANDSHAKE_TIMEOUT
                })
                .map(|(token, _)| *token)
                .collect();
            for token in expired {
                self.counters.handshake_timeouts =
                    self.counters.handshake_timeouts.saturating_add(1);
                self.close_connection(token);
            }
        }
        if self.accept_retry_at.is_some_and(|at| Instant::now() >= at) {
            self.accept_retry_at = None;
            if let Some(listener) = &mut self.listener {
                self.poll
                    .registry()
                    .register(listener, LISTENER_TOKEN, Interest::READABLE)?;
                self.defer(ReadyEvent::continuation(LISTENER_TOKEN, true, false));
            }
        }
        // Coalesce kernel readiness and deferred work. Each token gets
        // at most one turn per iteration, even if both sources report it.
        self.ready.clear();
        for event in self.events.iter() {
            let event = ReadyEvent {
                token: event.token(),
                readable: event.is_readable(),
                writable: event.is_writable(),
                error: event.is_error(),
                read_closed: event.is_read_closed(),
                write_closed: event.is_write_closed(),
            };
            self.deferred
                .entry(event.token)
                .and_modify(|old| old.merge(event))
                .or_insert(event);
        }
        self.ready
            .extend(self.deferred.drain().map(|(_, event)| event));

        for index in 0..self.ready.len() {
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            let event = self.ready[index];
            match event.token {
                LISTENER_TOKEN => self.accept_ready()?,
                WAKE_TOKEN => self.handle_wake()?,
                token => self.connection_ready(token, event)?,
            }
            self.ensure_healthy()?;
        }

        if !self.shutdown.load(Ordering::Acquire) {
            self.sweep_expired_if_due()?;
        }
        self.counters.polls_completed = self.counters.polls_completed.saturating_add(1);
        Ok(())
    }

    fn poll_timeout(&self, requested: Option<Duration>) -> Option<Duration> {
        if !self.deferred.is_empty() {
            return Some(Duration::ZERO);
        }
        let now = Instant::now();
        let mut deadline = self.next_expiry_sweep.saturating_duration_since(now);
        if let Some(at) = self.accept_retry_at {
            deadline = deadline.min(at.saturating_duration_since(now));
        }
        if self.mutual_tls.is_some() {
            for connection in self.connections.values().filter(|c| c.handshake_pending()) {
                deadline = deadline.min(
                    TLS_HANDSHAKE_TIMEOUT
                        .saturating_sub(now.saturating_duration_since(connection.accepted_at)),
                );
            }
        }
        Some(requested.map_or(deadline, |requested| requested.min(deadline)))
    }

    fn sweep_expired_if_due(&mut self) -> Result<(), CacheServerError> {
        let now = Instant::now();
        if now < self.next_expiry_sweep {
            return Ok(());
        }

        let now_ms = self.clock.now_ms();
        let budget = self.config.max_expiry_items_per_sweep;
        match &mut self.backend {
            CacheBackend::Memory(store) => {
                store.purge_expired(now_ms, budget);
            }
            #[cfg(unix)]
            CacheBackend::Durable { cache, .. } => {
                cache
                    .purge_expired(now_ms, budget)
                    .map_err(CacheServerError::Durability)?;
            }
        }
        self.next_expiry_sweep = now + self.config.expiry_sweep_interval;
        Ok(())
    }

    fn accept_ready(&mut self) -> Result<(), CacheServerError> {
        if self.accept_retry_at.is_some() {
            return Ok(());
        }
        for _ in 0..ACCEPT_QUANTUM {
            if self.shutdown.load(Ordering::Acquire) {
                return Ok(());
            }
            let Some(listener) = &self.listener else {
                return Ok(());
            };
            match listener.accept() {
                Ok((stream, peer)) => {
                    if self.connections.len() >= self.config.max_connections
                        || (self.mutual_tls.is_some()
                            && self.connections.len() >= TLS_CONNECTION_LIMIT)
                    {
                        self.counters.rejected_limit =
                            self.counters.rejected_limit.saturating_add(1);
                        drop(stream);
                        continue;
                    }

                    if stream.set_nodelay(true).is_err() {
                        continue;
                    }
                    let Some(peer) = self
                        .buffer_budget
                        .admit_peer(peer.ip(), self.config.max_connections_per_peer)
                    else {
                        self.counters.rejected_peer = self.counters.rejected_peer.saturating_add(1);
                        continue;
                    };
                    let token = self.allocate_connection_token();
                    let mut connection =
                        CacheConnection::new(stream, self.buffer_budget.lease(), peer);
                    if let Some(tls) = &self.mutual_tls {
                        // Charge before constructing Rustls state, even for a
                        // peer that never sends a ClientHello. The same lease
                        // holds RESP allocations and releases on every close.
                        if !connection.lease.reserve_tls() {
                            self.counters.rejected_tls_budget =
                                self.counters.rejected_tls_budget.saturating_add(1);
                            continue;
                        }
                        let mut session = rustls::ServerConnection::new(tls.config.clone())
                            .map_err(io::Error::other)?;
                        session.set_buffer_limit(Some(TLS_BUFFER_LIMIT));
                        connection.tls = Some(Box::new(session));
                    }
                    self.poll.registry().register(
                        &mut connection.stream,
                        token,
                        Interest::READABLE,
                    )?;
                    self.connections.insert(token, connection);
                    self.counters.accepted = self.counters.accepted.saturating_add(1);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    continue
                }
                Err(error) if is_accept_resource_exhaustion(&error) => {
                    self.counters.accept_resource_errors =
                        self.counters.accept_resource_errors.saturating_add(1);
                    // Remove listener readiness during backoff to avoid a
                    // tight loop when the backlog is nonempty and accept
                    // cannot allocate a descriptor. Existing sockets remain.
                    if let Some(listener) = &mut self.listener {
                        self.poll.registry().deregister(listener)?;
                    }
                    self.accept_retry_at = Some(Instant::now() + ACCEPT_BACKOFF);
                    self.deferred.remove(&LISTENER_TOKEN);
                    return Ok(());
                }
                Err(error) => return Err(CacheServerError::Io(error)),
            }
        }
        self.defer(ReadyEvent::continuation(LISTENER_TOKEN, true, false));
        Ok(())
    }

    fn defer(&mut self, event: ReadyEvent) {
        self.deferred
            .entry(event.token)
            .and_modify(|old| old.merge(event))
            .or_insert(event);
    }

    fn handle_wake(&mut self) -> Result<(), CacheServerError> {
        match &mut self.backend {
            CacheBackend::Memory(store) => {
                if self.inbox.drain(store, self.config.max_inbox_batch)
                    == self.config.max_inbox_batch
                {
                    self.defer(ReadyEvent::continuation(WAKE_TOKEN, false, false));
                }
            }
            #[cfg(unix)]
            CacheBackend::Durable { .. } => {
                if self.inbox.reject_pending(self.config.max_inbox_batch)
                    == self.config.max_inbox_batch
                {
                    self.defer(ReadyEvent::continuation(WAKE_TOKEN, false, false));
                }
            }
        }
        Ok(())
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
            Ok((read_again, write_again)) => {
                self.update_interest(token, &mut connection)?;
                let resume = read_again || write_again || connection.commands_remaining != 0;
                self.connections.insert(token, connection);
                if resume {
                    self.defer(ReadyEvent::continuation(token, read_again, write_again));
                }
            }
            Err(ConnectionAction::Close) => {
                self.counters.connection_errors = self.counters.connection_errors.saturating_add(1);
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
    ) -> Result<(bool, bool), ConnectionAction> {
        let mut read_again = readable;
        // Finish the preflighted batch before adding more input. Preserve a
        // readable continuation if a new edge arrived while dispatch yielded.
        if readable && connection.commands_remaining == 0 && !self.shutdown.load(Ordering::Acquire)
        {
            read_again = self
                .read_connection(connection)
                .map_err(|_| ConnectionAction::Close)?;
        }

        if !connection.handshake_pending() {
            self.process_input(connection)?;
        }

        if connection.pending_output() > self.config.max_output_buffer {
            return Err(ConnectionAction::Close);
        }

        let mut write_again = false;
        if writable || connection.has_pending_wire() {
            let mut remaining = WRITE_QUANTUM;
            Self::flush_connection_while(connection, || {
                if remaining == 0 {
                    return false;
                }
                remaining -= 1;
                true
            })
            .map_err(|_| ConnectionAction::Close)?;
            write_again = remaining == 0 && connection.has_pending_wire();
        }

        // Reclaim only empty allocations. A yielded batch retains its input.
        connection.lease.release_empty(&mut connection.input);
        connection.lease.release_empty(&mut connection.output);
        Ok((read_again, write_again))
    }

    // True means there may be unread kernel/TLS data; schedule another turn.
    fn read_connection(&self, connection: &mut CacheConnection) -> io::Result<bool> {
        connection.compact_input();
        let mut scratch = [0u8; IO_CHUNK_BYTES];
        let mut remaining = READ_QUANTUM;

        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return Ok(false);
            }
            match connection.read_plaintext(&mut scratch, &mut remaining) {
                Ok(None) => return Ok(true),
                Ok(Some(0)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "cache client closed connection",
                    ));
                }
                Ok(Some(read)) => {
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
                    if connection.input_start != 0 {
                        connection.input.copy_within(connection.input_start.., 0);
                        connection
                            .input
                            .truncate(connection.input.len() - connection.input_start);
                        connection.input_start = 0;
                    }
                    let required = connection.input.len().saturating_add(read);
                    connection.lease.reserve(
                        &mut connection.input,
                        required,
                        self.config.max_input_buffer,
                    )?;
                    connection.input.extend_from_slice(&scratch[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error),
            }
        }
    }

    fn process_input(&mut self, connection: &mut CacheConnection) -> Result<(), ConnectionAction> {
        // Bound the number of complete commands in a buffered read batch
        // before executing any of that batch. A partial trailing frame is
        // retained for a later read, not counted as a complete command.
        if connection.commands_remaining == 0 {
            let mut remaining = &connection.input[connection.input_start..];
            let mut depth = 0;
            while !remaining.is_empty() {
                if self.shutdown.load(Ordering::Acquire) {
                    return Ok(());
                }
                let Some((_, consumed)) =
                    parse_command(remaining).map_err(|_| ConnectionAction::Close)?
                else {
                    break;
                };
                if depth == self.config.max_pipeline_depth {
                    return Err(ConnectionAction::Close);
                }
                depth += 1;
                remaining = &remaining[consumed..];
            }
            connection.commands_remaining = depth;
        }
        for _ in 0..COMMAND_QUANTUM {
            if self.shutdown.load(Ordering::Acquire) || connection.commands_remaining == 0 {
                connection.compact_input();
                return Ok(());
            }
            let input = &connection.input[connection.input_start..];
            let Some((command, consumed)) =
                parse_command(input).map_err(|_| ConnectionAction::Close)?
            else {
                return Err(ConnectionAction::Close);
            };
            let now = self.clock.now_ms();
            let sizing_store = match &self.backend {
                CacheBackend::Memory(store) => store,
                #[cfg(unix)]
                CacheBackend::Durable { cache, .. } => cache
                    .store_for_sizing()
                    .map_err(|_| ConnectionAction::Close)?,
            };
            let bound = self
                .dispatcher
                .redirect_reply_bound(sizing_store, command, now);
            // The redirect-only server never queues asynchronous replies.
            // Compact already sent bytes and reserve before executing, avoiding
            // the response pipeline's second copy and large MGET amplification.
            if connection.output_start != 0 {
                connection.output.copy_within(connection.output_start.., 0);
                connection
                    .output
                    .truncate(connection.output.len() - connection.output_start);
                connection.output_start = 0;
            }
            let required = connection.output.len().saturating_add(bound);
            connection
                .lease
                .reserve(
                    &mut connection.output,
                    required,
                    self.config.max_output_buffer,
                )
                .map_err(|_| ConnectionAction::Close)?;
            let reserved_capacity = connection.output.capacity();
            match &mut self.backend {
                CacheBackend::Memory(store) => {
                    let outcome = self
                        .dispatcher
                        .dispatch_frame(store, input, now, &mut connection.output)
                        .map_err(|_| ConnectionAction::Close)?;
                    match outcome {
                        Some(CacheDispatchOutcome::Executed { consumed: actual })
                        | Some(CacheDispatchOutcome::Redirected {
                            consumed: actual, ..
                        }) if actual == consumed => {}
                        _ => return Err(ConnectionAction::Close),
                    }
                }
                #[cfg(unix)]
                CacheBackend::Durable { cache, anchor } => {
                    if !self
                        .dispatcher
                        .redirect_only(command, &mut connection.output)
                        .map_err(|_| ConnectionAction::Close)?
                    {
                        let time =
                            durable_clock(*anchor, now).map_err(|_| ConnectionAction::Close)?;
                        super::resp_cache::execute_durable_command(
                            cache,
                            command,
                            time,
                            &mut connection.output,
                        )
                        .map_err(|_| ConnectionAction::Close)?;
                    }
                }
            }
            // Fail closed before releasing a lease if a response estimator
            // regresses. Never let an uncharged allocation escape this drive.
            if connection.output.len() > required
                || connection.output.capacity() > reserved_capacity
            {
                return Err(ConnectionAction::Close);
            }
            self.counters.commands_dispatched = self.counters.commands_dispatched.saturating_add(1);
            connection.input_start += consumed;
            connection.commands_remaining -= 1;
            // Check between pipelined commands as well as between poll cycles.
            #[cfg(unix)]
            if !self.shutdown.load(Ordering::Acquire) {
                self.maybe_checkpoint(self.clock.now_ms())
                    .map_err(|_| ConnectionAction::Close)?;
            }
        }
        connection.compact_input();
        Ok(())
    }

    #[cfg(test)]
    fn flush_connection(connection: &mut CacheConnection) -> io::Result<()> {
        Self::flush_connection_while(connection, || true)
    }

    fn flush_connection_while(
        connection: &mut CacheConnection,
        mut can_write: impl FnMut() -> bool,
    ) -> io::Result<()> {
        if let Some(tls) = &mut connection.tls {
            loop {
                if !can_write() {
                    break;
                }
                if tls.wants_write() {
                    match tls.write_tls(&mut connection.stream) {
                        Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                        Ok(_) => continue,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
                if tls.is_handshaking() || connection.output_start == connection.output.len() {
                    break;
                }
                let end = (connection.output_start + IO_CHUNK_BYTES).min(connection.output.len());
                match tls
                    .writer()
                    .write(&connection.output[connection.output_start..end])
                {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => connection.output_start += n,
                    Err(e) => return Err(e),
                }
            }
            connection.compact_output();
            return Ok(());
        }
        while connection.output_start < connection.output.len() && can_write() {
            let end = (connection.output_start + IO_CHUNK_BYTES).min(connection.output.len());
            match connection
                .stream
                .write(&connection.output[connection.output_start..end])
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
        let wants_writable = connection.has_pending_wire();
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
        self.deferred.remove(&token);
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

fn is_accept_resource_exhaustion(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(
            error.raw_os_error(),
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
        )
    }
    #[cfg(windows)]
    {
        // WSAEMFILE, WSAENOBUFS.
        matches!(error.raw_os_error(), Some(10024 | 10055))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

fn validate_config(config: &CacheServerConfig) -> Result<(), CacheServerError> {
    if config.max_total_connection_bytes == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_total_connection_bytes must be non-zero",
        ));
    }
    if config.max_connections_per_peer == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_connections_per_peer must be non-zero",
        ));
    }
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
        server
            .store_mut()
            .unwrap()
            .set_bytes(b"ttl", b"value", Some(1), now)
            .unwrap();

        std::thread::sleep(Duration::from_millis(3));
        server.poll_once(Some(Duration::from_millis(1))).unwrap();

        let now = server.clock.now_ms();
        assert!(server.store_mut().unwrap().get(b"ttl", now).is_none());
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

    #[test]
    fn shared_buffer_budget_rejects_growth_and_recovers_on_drop() {
        let budget = CacheBufferBudget::new(64);
        let mut a = budget.lease();
        let mut b = budget.lease();
        let mut first = Vec::new();
        let mut second = Vec::new();
        a.reserve(&mut first, 40, 64).unwrap();
        assert!(b.reserve(&mut second, 40, 64).is_err());
        assert_eq!(second.capacity(), 0);
        assert_eq!(budget.used_bytes(), first.capacity());
        drop(first);
        drop(a);
        b.reserve(&mut second, 40, 64).unwrap();
        assert_eq!(budget.used_bytes(), second.capacity());
    }

    #[test]
    fn repeated_large_mget_is_rejected_before_reply_allocation() {
        let mut server = build_server();
        server.config.max_output_buffer = 256;
        server
            .store_mut()
            .unwrap()
            .set_bytes(b"k", &[b'x'; 120], None, 0)
            .unwrap();
        let mut client = StdTcpStream::connect(server.local_addr().unwrap()).unwrap();
        client
            .write_all(b"*4\r\n$4\r\nMGET\r\n$1\r\nk\r\n$1\r\nk\r\n$1\r\nk\r\n")
            .unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        assert!(server.store_mut().unwrap().exists(b"k", 0));
    }

    #[test]
    fn network_clients_share_budget_and_disconnect_releases_capacity() {
        let mut server = build_server();
        server.config.max_input_buffer = 256;
        server.config.max_output_buffer = 256;
        server.buffer_budget = CacheBufferBudget::new(512);
        let addr = server.local_addr().unwrap();
        let ping = b"*1\r\n$4\r\nPING\r\n";

        // Incomplete frames are live data and must retain their charges.
        let mut first = StdTcpStream::connect(addr).unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        first.write_all(&ping[..5]).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(server.buffer_budget.used_bytes(), 256);

        let mut second = StdTcpStream::connect(addr).unwrap();
        second.write_all(&ping[..5]).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(server.connection_count(), 2);
        assert_eq!(server.buffer_budget.used_bytes(), 512);

        let mut third = StdTcpStream::connect(addr).unwrap();
        third.write_all(ping).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(server.connection_count(), 2);
        assert_eq!(server.buffer_budget.used_bytes(), 512);

        drop(second);
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(server.connection_count(), 1);
        assert_eq!(server.buffer_budget.used_bytes(), 256);

        first.write_all(&ping[5..]).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        let mut response = [0; 7];
        first.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"+PONG\r\n");
        assert_eq!(server.connection_count(), 1);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
    }

    #[test]
    fn test_idle_clients_release_capacity_without_disconnecting() {
        let mut server = build_server();
        server.config.max_input_buffer = 256;
        server.config.max_output_buffer = 256;
        server.buffer_budget = CacheBufferBudget::new(512);
        let addr = server.local_addr().unwrap();
        let ping = b"*1\r\n$4\r\nPING\r\n";
        let mut clients = Vec::new();
        for _ in 0..4 {
            let mut client = StdTcpStream::connect(addr).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            client.write_all(ping).unwrap();
            for _ in 0..8 {
                server.poll_once(Some(Duration::from_millis(5))).unwrap();
            }
            let mut response = [0; 7];
            client.read_exact(&mut response).unwrap();
            assert_eq!(&response, b"+PONG\r\n");
            assert_eq!(server.buffer_budget.used_bytes(), 0);
            clients.push(client);
        }
        assert_eq!(server.connection_count(), 4);
        clients[0].write_all(ping).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        let mut response = [0; 7];
        clients[0].read_exact(&mut response).unwrap();
        assert_eq!(&response, b"+PONG\r\n");
        assert_eq!(server.buffer_budget.used_bytes(), 0);
    }

    #[test]
    fn test_release_empty_is_idempotent_and_preserves_live_bytes() {
        let budget = CacheBufferBudget::new(128);
        let mut lease = budget.lease();
        let mut input = Vec::new();
        let mut output = Vec::new();
        lease.reserve(&mut input, 32, 64).unwrap();
        lease.reserve(&mut output, 32, 64).unwrap();
        output.extend_from_slice(b"unsent");
        lease.release_empty(&mut output);
        assert_eq!(&output, b"unsent");
        assert_eq!(budget.used_bytes(), 128);
        lease.release_empty(&mut input);
        lease.release_empty(&mut input);
        assert_eq!(input.capacity(), 0);
        assert_eq!(budget.used_bytes(), 64);
        output.clear();
        lease.release_empty(&mut output);
        assert_eq!(budget.used_bytes(), 0);
        lease.reserve(&mut input, 32, 64).unwrap();
        assert_eq!(budget.used_bytes(), 64);
        drop(input);
        drop(output);
        drop(lease);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[cfg(unix)]
    struct DurableTestDir(PathBuf);
    #[cfg(unix)]
    impl DurableTestDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "nulang-durable-reactor-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }
    #[cfg(unix)]
    impl Drop for DurableTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(unix)]
    fn build_durable_server(
        path: &std::path::Path,
        sync: WalSync,
        limits: WalLimits,
    ) -> CacheShardServer {
        let placement = CacheSlotMap::new_local(1, 1).unwrap();
        let owner = placement.owner_for_slot(0).unwrap();
        let (channels, mut inboxes) = CacheDispatchChannels::new(1, 32).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(owner, CacheAdvertisedEndpoint::new("127.0.0.1", 7000));
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);
        CacheShardServer::bind_durable(
            "127.0.0.1:0".parse().unwrap(),
            dispatcher,
            inboxes.remove(0),
            CacheDurableConfig {
                path: path.to_path_buf(),
                cache_limits: CacheLimits::default(),
                wal_limits: limits,
                sync,
            },
            CacheServerConfig::default(),
            CacheServerClock::new(),
        )
        .unwrap()
    }
    #[cfg(unix)]
    fn connect_client(server: &CacheShardServer) -> StdTcpStream {
        let client = StdTcpStream::connect(server.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client
    }
    #[cfg(unix)]
    fn roundtrip(
        server: &mut CacheShardServer,
        client: &mut StdTcpStream,
        request: &[u8],
        expected: &[u8],
    ) {
        client.write_all(request).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        let mut response = vec![0; expected.len()];
        client.read_exact(&mut response).unwrap();
        assert_eq!(response, expected);
    }

    #[cfg(unix)]
    #[test]
    fn test_durable_reactor_restarts_and_preserves_mutations() {
        let tmp = DurableTestDir::new();
        for mode in [
            super::super::cache_wal::WalSync::Buffered,
            super::super::cache_wal::WalSync::Data,
        ] {
            let journal = tmp.0.join(format!("{mode:?}.wal"));
            {
                let mut server = build_durable_server(
                    &journal,
                    mode,
                    super::super::cache_wal::WalLimits::default(),
                );
                assert!(server.store_mut().is_none());
                let mut client = connect_client(&server);
                roundtrip(&mut server, &mut client, b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$2\r\n41\r\n*2\r\n$4\r\nINCR\r\n$3\r\nkey\r\n*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n", b"+OK\r\n:42\r\n$2\r\n42\r\n");
            }
            let mut server = build_durable_server(
                &journal,
                mode,
                super::super::cache_wal::WalLimits::default(),
            );
            let mut client = connect_client(&server);
            roundtrip(
                &mut server,
                &mut client,
                b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n",
                b"$2\r\n42\r\n",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_durable_reactor_poison_closes_every_client_and_recovers() {
        let tmp = DurableTestDir::new();
        let journal = tmp.0.join("failure.wal");
        let mut server = build_durable_server(&journal, WalSync::Data, WalLimits::default());
        let mut first = connect_client(&server);
        let mut second = connect_client(&server);
        roundtrip(
            &mut server,
            &mut first,
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\nold\r\n",
            b"+OK\r\n",
        );
        roundtrip(
            &mut server,
            &mut second,
            b"*1\r\n$4\r\nPING\r\n",
            b"+PONG\r\n",
        );
        assert_eq!(server.connection_count(), 2);
        if let CacheBackend::Durable { cache, .. } = &mut server.backend {
            cache.fail_next_append();
        }
        first
            .write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\nnew\r\n*2\r\n$3\r\nGET\r\n$1\r\nk\r\n")
            .unwrap();
        let mut failed = false;
        for _ in 0..8 {
            match server.poll_once(Some(Duration::from_millis(5))) {
                Err(CacheServerError::DurableShardPoisoned) => {
                    failed = true;
                    break;
                }
                other => other.unwrap(),
            }
        }
        assert!(failed);
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        assert!(matches!(
            server.poll_once(Some(Duration::ZERO)),
            Err(CacheServerError::DurableShardPoisoned)
        ));
        for client in [&mut first, &mut second] {
            let mut byte = [0];
            match client.read(&mut byte) {
                Ok(0) => {}
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
                other => panic!("poisoned shard served data or remained open: {other:?}"),
            }
        }
        drop(server);
        let mut recovered = build_durable_server(&journal, WalSync::Data, WalLimits::default());
        let mut client = connect_client(&recovered);
        roundtrip(
            &mut recovered,
            &mut client,
            b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n",
            b"$3\r\nold\r\n",
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_durable_reactor_limit_error_does_not_poison_or_mutate() {
        let tmp = DurableTestDir::new();
        let journal = tmp.0.join("limit.wal");
        let mut server = build_durable_server(
            &journal,
            WalSync::Data,
            WalLimits {
                max_record_bytes: 128,
                max_file_bytes: 96,
            },
        );
        let mut client = connect_client(&server);
        roundtrip(
            &mut server,
            &mut client,
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
            b"+OK\r\n",
        );
        roundtrip(
            &mut server,
            &mut client,
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\nnew\r\n*2\r\n$3\r\nGET\r\n$1\r\nk\r\n",
            b"-ERR cache persistence unavailable\r\n$1\r\nv\r\n",
        );
        assert_eq!(server.connection_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_durable_reactor_batches_and_ttl_recovery() {
        let tmp = DurableTestDir::new();
        let journal = tmp.0.join("batch.wal");
        let mut server = build_durable_server(&journal, WalSync::Data, WalLimits::default());
        let mut client = connect_client(&server);
        roundtrip(&mut server, &mut client,
            b"*5\r\n$4\r\nMSET\r\n$4\r\na{t}\r\n$1\r\nx\r\n$4\r\nb{t}\r\n$1\r\ny\r\n*3\r\n$4\r\nMGET\r\n$4\r\na{t}\r\n$4\r\nb{t}\r\n*3\r\n$6\r\nEXPIRE\r\n$4\r\na{t}\r\n$2\r\n60\r\n*2\r\n$3\r\nDEL\r\n$4\r\nb{t}\r\n",
            b"+OK\r\n*2\r\n$1\r\nx\r\n$1\r\ny\r\n:1\r\n:1\r\n");
        roundtrip(
            &mut server,
            &mut client,
            b"*3\r\n$6\r\nEXISTS\r\n$4\r\na{t}\r\n$4\r\nb{t}\r\n",
            b":1\r\n",
        );
        drop(server);
        let mut recovered = build_durable_server(&journal, WalSync::Data, WalLimits::default());
        let mut client = connect_client(&recovered);
        roundtrip(
            &mut recovered,
            &mut client,
            b"*3\r\n$4\r\nMGET\r\n$4\r\na{t}\r\n$4\r\nb{t}\r\n",
            b"*2\r\n$1\r\nx\r\n$-1\r\n",
        );
        if let CacheBackend::Durable { cache, .. } = &mut recovered.backend {
            assert!(
                matches!(cache.ttl(b"a{t}", recovered.clock.now_ms()).unwrap(), super::super::cache::CacheTtl::RemainingMs(ms) if ms > 0 && ms <= 60_000)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_durable_reactor_checkpoint_recovers_and_resumes_tcp_service() {
        let tmp = DurableTestDir::new();
        let journal = tmp.0.join("checkpoint.wal");
        let mut server = build_durable_server(&journal, WalSync::Data, WalLimits::default());
        let mut client = connect_client(&server);
        for _ in 0..4 {
            roundtrip(
                &mut server,
                &mut client,
                b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
                b"+OK\r\n",
            );
        }
        let report = server.checkpoint().unwrap();
        assert_eq!(report.entries, 1);
        assert!(report.after_bytes < report.before_bytes);
        roundtrip(
            &mut server,
            &mut client,
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\nnew\r\n",
            b"+OK\r\n",
        );
        drop(server);
        let mut recovered = build_durable_server(&journal, WalSync::Data, WalLimits::default());
        let mut client = connect_client(&recovered);
        roundtrip(
            &mut recovered,
            &mut client,
            b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n",
            b"$3\r\nnew\r\n",
        );
    }

    #[cfg(unix)]
    #[test]
    fn automatic_checkpoint_skips_ineffective_snapshot_and_honors_retry_cooldown() {
        let tmp = DurableTestDir::new();
        let journal = tmp.0.join("ineffective.wal");
        let mut server = build_durable_server(
            &journal,
            WalSync::Data,
            WalLimits {
                max_record_bytes: 128,
                max_file_bytes: 512,
            },
        );
        let mut client = connect_client(&server);
        for key in *b"abcd" {
            let mut wire = b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\nv\r\n".to_vec();
            let index = wire.iter().position(|&byte| byte == b'a').unwrap();
            wire[index] = key;
            roundtrip(&mut server, &mut client, &wire, b"+OK\r\n");
        }
        let old = std::fs::read(&journal).unwrap();
        server
            .enable_automatic_checkpoint(AutomaticCheckpointConfig {
                trigger_percent: 40,
                min_savings_percent: 10,
                retry_cooldown_ms: 5000,
            })
            .unwrap();
        let now = server.clock.now_ms();
        server.maybe_checkpoint(now).unwrap();
        assert_eq!(
            server
                .automatic_checkpoint_stats()
                .unwrap()
                .skipped_ineffective,
            1
        );
        server.maybe_checkpoint(now + 4999).unwrap();
        assert_eq!(
            server
                .automatic_checkpoint_stats()
                .unwrap()
                .skipped_ineffective,
            1
        );
        server.maybe_checkpoint(now + 5000).unwrap();
        let stats = server.automatic_checkpoint_stats().unwrap();
        assert_eq!(stats.skipped_ineffective, 2);
        assert_eq!(stats.completed, 0);
        assert_eq!(std::fs::read(&journal).unwrap(), old);
    }

    #[cfg(unix)]
    #[test]
    fn automatic_checkpoint_retries_prepublication_failure_without_poisoning() {
        let tmp = DurableTestDir::new();
        let parent = tmp.0.join("journal");
        let moved = tmp.0.join("moved");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("cache.wal");
        let mut server = build_durable_server(
            &path,
            WalSync::Data,
            WalLimits {
                max_record_bytes: 128,
                max_file_bytes: 512,
            },
        );
        let mut client = connect_client(&server);
        for _ in 0..5 {
            roundtrip(
                &mut server,
                &mut client,
                b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
                b"+OK\r\n",
            );
        }
        let old = std::fs::read(&path).unwrap();
        server
            .enable_automatic_checkpoint(AutomaticCheckpointConfig {
                trigger_percent: 40,
                min_savings_percent: 10,
                retry_cooldown_ms: 5000,
            })
            .unwrap();
        // Simulate an inaccessible destination before the replacement is created.
        std::fs::rename(&parent, &moved).unwrap();
        let now = server.clock.now_ms();
        server.maybe_checkpoint(now).unwrap();
        assert_eq!(server.automatic_checkpoint_stats().unwrap().failed, 1);
        assert_eq!(std::fs::read(moved.join("cache.wal")).unwrap(), old);
        std::fs::rename(&moved, &parent).unwrap();
        server.maybe_checkpoint(now + 4999).unwrap();
        assert_eq!(server.automatic_checkpoint_stats().unwrap().completed, 0);
        server.maybe_checkpoint(now + 5000).unwrap();
        let stats = server.automatic_checkpoint_stats().unwrap();
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.failed, 1);
        assert!(stats.reclaimed_bytes > 0);
        roundtrip(
            &mut server,
            &mut client,
            b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n",
            b"$1\r\nv\r\n",
        );
    }

    #[test]
    fn graceful_shutdown_drains_encoded_replies_without_executing_pending_input() {
        let mut server = build_server();
        let address = server.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == 1 {
                break;
            }
        }
        let connection = server.connections.values_mut().next().unwrap();
        let pending = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
        connection
            .lease
            .reserve(&mut connection.input, pending.len(), 1024)
            .unwrap();
        connection.input.extend_from_slice(pending);
        connection
            .lease
            .reserve(&mut connection.output, 7, 1024)
            .unwrap();
        connection.output.extend_from_slice(b"+PONG\r\n");

        server.control().shutdown();
        server
            .run_with_shutdown_timeout(Duration::from_secs(1))
            .unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"+PONG\r\n");
        assert!(server.store_mut().unwrap().get(b"k", 0).is_none());
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        assert!(server.local_addr().is_err());
        assert!(mio::net::TcpListener::bind(address).is_ok());
    }

    #[test]
    fn zero_shutdown_drain_drops_pending_replies_and_releases_budget() {
        let mut server = build_server();
        let mut client = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == 1 {
                break;
            }
        }
        let connection = server.connections.values_mut().next().unwrap();
        connection
            .lease
            .reserve(&mut connection.output, 7, 1024)
            .unwrap();
        connection.output.extend_from_slice(b"+PONG\r\n");
        server.control().shutdown();
        server.run_with_shutdown_timeout(Duration::ZERO).unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert!(reply.is_empty());
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        assert_eq!(server.connection_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_deadline_releases_a_backpressured_connection() {
        use std::os::fd::AsRawFd;
        let mut server = build_server();
        let client = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == 1 {
                break;
            }
        }
        let connection = server.connections.values_mut().next().unwrap();
        let size: libc::c_int = 4096;
        // SAFETY: valid live socket, pointer to correctly sized integer option.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    connection.stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        let bytes = 1024 * 1024;
        connection
            .lease
            .reserve(&mut connection.output, bytes, bytes)
            .unwrap();
        connection.output.resize(bytes, b'x');
        CacheShardServer::flush_connection(connection).unwrap();
        assert!(connection.pending_output() > 0);
        let timeout = Duration::from_millis(30);
        server.control().shutdown();
        let started = Instant::now();
        server.run_with_shutdown_timeout(timeout).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        drop(client);
    }

    #[test]
    fn shutdown_flush_checks_write_permission_before_sending() {
        let mut server = build_server();
        let mut client = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == 1 {
                break;
            }
        }
        let connection = server.connections.values_mut().next().unwrap();
        connection
            .lease
            .reserve(&mut connection.output, 7, 1024)
            .unwrap();
        connection.output.extend_from_slice(b"+PONG\r\n");
        CacheShardServer::flush_connection_while(connection, || false).unwrap();
        assert_eq!(connection.pending_output(), 7);
        CacheShardServer::flush_connection_while(connection, || true).unwrap();
        let mut reply = [0; 7];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"+PONG\r\n");
    }

    #[cfg(unix)]
    #[test]
    fn clock_persistence_failure_during_resp_read_stops_shard() {
        let tmp = DurableTestDir::new();
        let journal = tmp.0.join("clock.wal");
        let mut server = build_durable_server(&journal, WalSync::Data, WalLimits::default());
        let mut client = connect_client(&server);
        roundtrip(
            &mut server,
            &mut client,
            b"*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n$2\r\nPX\r\n$5\r\n60000\r\n",
            b"+OK\r\n",
        );
        let clock_path = tmp.0.join("clock.wal.clock");
        std::fs::remove_file(&clock_path).unwrap();
        std::fs::create_dir(&clock_path).unwrap();
        // Ensure the next time-sensitive command advances beyond the SET floor.
        std::thread::sleep(Duration::from_millis(2));
        client.write_all(b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n").unwrap();
        let mut failed = false;
        for _ in 0..8 {
            match server.poll_once(Some(Duration::from_millis(5))) {
                Err(CacheServerError::DurableShardPoisoned) => {
                    failed = true;
                    break;
                }
                Ok(()) => {}
                other => panic!("unexpected reactor result {other:?}"),
            }
        }
        assert!(failed);
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        let mut byte = [0];
        match client.read(&mut byte) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("unexpected reply after poisoned clock: {other:?}"),
        }
    }

    #[test]
    fn overdepth_pipeline_is_rejected_before_mutation() {
        let mut server = build_server();
        server.config.max_pipeline_depth = 2;
        let mut client = StdTcpStream::connect(server.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let set = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
        client
            .write_all(
                &[
                    set.as_slice(),
                    b"*1\r\n$4\r\nPING\r\n",
                    b"*1\r\n$4\r\nPING\r\n",
                ]
                .concat(),
            )
            .unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert!(server.store_mut().unwrap().get(b"k", 0).is_none());
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
    }

    #[test]
    fn tls_stalled_handshakes_bound_poll_timeout_and_release_admission() {
        let mut server = build_server();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        server
            .enable_mutual_tls(
                CacheMutualTls::from_pem(
                    cert.cert.pem().as_bytes(),
                    cert.cert.pem().as_bytes(),
                    cert.signing_key.serialize_pem().as_bytes(),
                )
                .unwrap(),
            )
            .unwrap();
        server.config.max_connections = 2;
        let addr = server.local_addr().unwrap();
        let _first = StdTcpStream::connect(addr).unwrap();
        let _second = StdTcpStream::connect(addr).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == 2 {
                break;
            }
        }
        assert_eq!(server.connection_count(), 2);
        assert_eq!(server.tls_reserved_bytes(), 2 * TLS_RESERVATION_BYTES);
        assert_eq!(server.buffer_budget.used_bytes(), 2 * TLS_RESERVATION_BYTES);
        let mut excess = StdTcpStream::connect(addr).unwrap();
        excess
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        match excess.read(&mut [0]) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("excess TLS connection not closed: {other:?}"),
        }
        assert_eq!(server.connection_count(), 2);
        assert_eq!(server.tls_reserved_bytes(), 2 * TLS_RESERVATION_BYTES);
        assert_eq!(server.buffer_budget.used_bytes(), 2 * TLS_RESERVATION_BYTES);
        for connection in server.connections.values_mut() {
            connection.accepted_at = Instant::now() - TLS_HANDSHAKE_TIMEOUT;
        }
        server.next_expiry_sweep = Instant::now() + Duration::from_secs(60);
        assert_eq!(server.poll_timeout(None), Some(Duration::ZERO));
        server.poll_once(Some(Duration::ZERO)).unwrap();
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.tls_reserved_bytes(), 0);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        let _replacement = StdTcpStream::connect(addr).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == 1 {
                break;
            }
        }
        assert_eq!(server.connection_count(), 1);
    }

    #[test]
    fn pipeline_limit_allows_exact_depth_and_partial_trailing_frame() {
        let mut server = build_server();
        server.config.max_pipeline_depth = 2;
        let mut client = StdTcpStream::connect(server.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client
            .write_all(b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPI")
            .unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        let mut responses = [0; 14];
        client.read_exact(&mut responses).unwrap();
        assert_eq!(&responses, b"+PONG\r\n+PONG\r\n");
        client.write_all(b"NG\r\n").unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        let mut response = [0; 7];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"+PONG\r\n");
        assert_eq!(server.buffer_budget.used_bytes(), 0);
    }

    #[test]
    fn accept_burst_yields_and_continues_without_new_connections() {
        let mut server = build_server();
        let address = server.local_addr().unwrap();
        let clients: Vec<_> = (0..40)
            .map(|_| StdTcpStream::connect(address).unwrap())
            .collect();
        server.poll_once(Some(Duration::from_millis(10))).unwrap();
        assert_eq!(server.connection_count(), 32);
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == clients.len() {
                break;
            }
        }
        assert_eq!(server.connection_count(), clients.len());
    }

    #[test]
    fn hot_pipeline_yields_to_other_client_and_resumes_in_order() {
        let mut server = build_server();
        let address = server.local_addr().unwrap();
        let mut hot = StdTcpStream::connect(address).unwrap();
        let mut cold = StdTcpStream::connect(address).unwrap();
        for client in [&hot, &cold] {
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
        }
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == 2 {
                break;
            }
        }
        hot.write_all(&b"*2\r\n$4\r\nINCR\r\n$1\r\nk\r\n".repeat(80))
            .unwrap();
        cold.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
        server.poll_once(Some(Duration::from_millis(10))).unwrap();
        assert_eq!(
            server.store_mut().unwrap().get(b"k", 0),
            Some(super::super::cache::CacheValueView::Integer(32))
        );
        server.poll_once(Some(Duration::from_millis(5))).unwrap();
        let mut pong = [0; 7];
        cold.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"+PONG\r\n");
        assert_eq!(
            server.store_mut().unwrap().get(b"k", 0),
            Some(super::super::cache::CacheValueView::Integer(64))
        );
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        let expected: String = (1..=80).map(|n| format!(":{n}\r\n")).collect();
        let mut reply = vec![0; expected.len()];
        hot.read_exact(&mut reply).unwrap();
        assert_eq!(reply, expected.as_bytes());
        assert_eq!(server.buffer_budget.used_bytes(), 0);
    }

    #[test]
    fn large_reply_flush_yields_and_resumes_without_new_client_request() {
        let mut server = build_server();
        let address = server.local_addr().unwrap();
        let mut client = StdTcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let value = vec![b'v'; 768 * 1024];
        server
            .store_mut()
            .unwrap()
            .set_bytes(b"large", &value, None, 0)
            .unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if server.connection_count() == 1 {
                break;
            }
        }
        client
            .write_all(b"*2\r\n$3\r\nGET\r\n$5\r\nlarge\r\n")
            .unwrap();
        server.poll_once(Some(Duration::from_millis(5))).unwrap();
        let connection = server.connections.values().next().unwrap();
        assert!(connection.pending_output() >= value.len() - WRITE_QUANTUM * IO_CHUNK_BYTES);
        let expected = [
            format!("{}{}\r\n", '$', value.len()).as_bytes(),
            &value,
            b"\r\n",
        ]
        .concat();
        let reader = std::thread::spawn(move || {
            let mut actual = vec![0; expected.len()];
            client.read_exact(&mut actual).unwrap();
            assert_eq!(actual, expected);
        });
        for _ in 0..200 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
            if reader.is_finished() {
                break;
            }
        }
        reader.join().unwrap();
        assert_eq!(server.buffer_budget.used_bytes(), 0);
    }

    #[test]
    fn tls_admission_reserves_shared_budget_before_handshake_and_releases_on_close() {
        const RESERVE: usize = 512 * 1024;
        let budget = CacheBufferBudget::new(RESERVE + 4096);
        let mut first = build_server();
        let mut second = build_server();
        first.buffer_budget = budget.clone();
        second.buffer_budget = budget.clone();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let tls = CacheMutualTls::from_pem(
            cert.cert.pem().as_bytes(),
            cert.cert.pem().as_bytes(),
            cert.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        first.enable_mutual_tls(tls.clone()).unwrap();
        second.enable_mutual_tls(tls).unwrap();

        let stalled = StdTcpStream::connect(first.local_addr().unwrap()).unwrap();
        for _ in 0..8 {
            first.poll_once(Some(Duration::from_millis(5))).unwrap();
            if first.connection_count() == 1 {
                break;
            }
        }
        assert_eq!(first.connection_count(), 1);
        assert_eq!(budget.used_bytes(), RESERVE);

        let mut denied = StdTcpStream::connect(second.local_addr().unwrap()).unwrap();
        denied
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for _ in 0..8 {
            second.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(second.connection_count(), 0);
        match denied.read(&mut [0; 1]) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("budget-denied connection not closed: {other:?}"),
        }
        assert_eq!(budget.used_bytes(), RESERVE);
        assert_eq!(
            budget
                .inner
                .peers
                .lock()
                .unwrap()
                .values()
                .copied()
                .sum::<usize>(),
            1
        );

        drop(stalled);
        for _ in 0..8 {
            first.poll_once(Some(Duration::from_millis(5))).unwrap();
            if first.connection_count() == 0 {
                break;
            }
        }
        assert_eq!(budget.used_bytes(), 0);

        let mut invalid = StdTcpStream::connect(second.local_addr().unwrap()).unwrap();
        for _ in 0..8 {
            second.poll_once(Some(Duration::from_millis(5))).unwrap();
            if second.connection_count() == 1 {
                break;
            }
        }
        assert_eq!(budget.used_bytes(), RESERVE);
        invalid.write_all(b"not tls at all").unwrap();
        for _ in 0..8 {
            second.poll_once(Some(Duration::from_millis(5))).unwrap();
            if second.connection_count() == 0 {
                break;
            }
        }
        assert_eq!(second.connection_count(), 0);
        assert_eq!(budget.used_bytes(), 0);

        let _replacement = StdTcpStream::connect(second.local_addr().unwrap()).unwrap();
        for _ in 0..8 {
            second.poll_once(Some(Duration::from_millis(5))).unwrap();
            if second.connection_count() == 1 {
                break;
            }
        }
        assert_eq!(budget.used_bytes(), RESERVE);
        second.control().shutdown();
        second.run_with_shutdown_timeout(Duration::ZERO).unwrap();
        assert_eq!(budget.used_bytes(), 0);
        assert!(budget.inner.peers.lock().unwrap().is_empty());
    }

    #[test]
    fn idle_buffer_release_retains_tls_charge_until_session_drop() {
        let budget = CacheBufferBudget::new(TLS_RESERVATION_BYTES + 4096);
        let mut lease = budget.lease();
        let mut input = Vec::new();
        let mut output = Vec::new();
        assert!(lease.reserve_tls());
        lease.reserve(&mut input, 128, 256).unwrap();
        lease.reserve(&mut output, 128, 256).unwrap();
        assert_eq!(
            budget.used_bytes(),
            TLS_RESERVATION_BYTES + input.capacity() + output.capacity()
        );
        input.extend_from_slice(b"partial");
        lease.release_empty(&mut input);
        assert_ne!(input.capacity(), 0);
        input.clear();
        lease.release_empty(&mut input);
        lease.release_empty(&mut output);
        assert_eq!(budget.used_bytes(), TLS_RESERVATION_BYTES);
        assert!(!budget.lease().reserve_tls());
        drop(lease);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn shared_tls_reservations_are_atomic_under_concurrent_admission() {
        use std::sync::Barrier;
        let budget = CacheBufferBudget::new(3 * TLS_RESERVATION_BYTES);
        let barrier = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let budget = budget.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let mut lease = budget.lease();
                let admitted = lease.reserve_tls();
                barrier.wait();
                barrier.wait();
                admitted
            }));
        }
        barrier.wait();
        assert_eq!(budget.used_bytes(), budget.limit_bytes());
        barrier.wait();
        let admitted = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 3);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn tls_configuration_rejects_budget_without_application_headroom() {
        let mut server = build_server();
        server.buffer_budget = CacheBufferBudget::new(TLS_RESERVATION_BYTES);
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let tls = CacheMutualTls::from_pem(
            cert.cert.pem().as_bytes(),
            cert.cert.pem().as_bytes(),
            cert.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            server.enable_mutual_tls(tls),
            Err(CacheServerError::InvalidConfig(_))
        ));
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        assert!(server.mutual_tls.is_none());
    }

    #[test]
    fn peer_quota_is_shared_across_reactors_and_reclaimed_after_disconnect() {
        let budget = CacheBufferBudget::new(1024 * 1024);
        let mut first = build_server();
        let mut second = build_server();
        first.buffer_budget = budget.clone();
        second.buffer_budget = budget.clone();
        first.config.max_connections_per_peer = 1;
        second.config.max_connections_per_peer = 1;
        let mut established = StdTcpStream::connect(first.local_addr().unwrap()).unwrap();
        established
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for _ in 0..8 {
            first.poll_once(Some(Duration::from_millis(5))).unwrap();
            if first.connection_count() == 1 {
                break;
            }
        }
        assert_eq!(first.connection_count(), 1);
        let mut denied = StdTcpStream::connect(second.local_addr().unwrap()).unwrap();
        denied
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for _ in 0..8 {
            second.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(second.connection_count(), 0);
        match denied.read(&mut [0; 1]) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("over-quota peer not closed: {other:?}"),
        }
        established.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
        for _ in 0..8 {
            first.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        let mut pong = [0; 7];
        established.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"+PONG\r\n");
        drop(established);
        for _ in 0..8 {
            first.poll_once(Some(Duration::from_millis(5))).unwrap();
            if first.connection_count() == 0 {
                break;
            }
        }
        assert!(budget.inner.peers.lock().unwrap().is_empty());
        let _replacement = StdTcpStream::connect(second.local_addr().unwrap()).unwrap();
        for _ in 0..8 {
            second.poll_once(Some(Duration::from_millis(5))).unwrap();
            if second.connection_count() == 1 {
                break;
            }
        }
        assert_eq!(second.connection_count(), 1);
        second.control().shutdown();
        second.run_with_shutdown_timeout(Duration::ZERO).unwrap();
        assert!(budget.inner.peers.lock().unwrap().is_empty());
    }

    #[test]
    fn peer_identity_normalizes_mapped_ipv4_and_keeps_distinct_ips_independent() {
        let budget = CacheBufferBudget::new(1024);
        let ip = "192.0.2.1".parse().unwrap();
        let mapped = "::ffff:192.0.2.1".parse().unwrap();
        let other = "192.0.2.2".parse().unwrap();
        let lease = budget.admit_peer(ip, 1).unwrap();
        assert!(budget.admit_peer(mapped, 1).is_none());
        let different = budget.admit_peer(other, 1).unwrap();
        assert_eq!(budget.inner.peers.lock().unwrap().len(), 2);
        drop(lease);
        let replacement = budget.admit_peer(mapped, 1).unwrap();
        drop(different);
        drop(replacement);
        assert!(budget.inner.peers.lock().unwrap().is_empty());
    }
}
