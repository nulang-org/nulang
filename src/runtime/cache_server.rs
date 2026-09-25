//! Dedicated readiness reactor for the RESP cache tier.
//!
//! The cache server is intentionally separate from the actor scheduler. Each
//! physical cache shard owns its listener, connections, CacheStore, and expiry
//! work on one thread. Cluster-aware clients are redirected to the owning shard
//! before execution, so the steady-state GET/SET path stays thread-local.

#![cfg(feature = "cache-server")]

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};

use super::cache::{CacheStore, CacheTtl, CacheValueView};
use super::cache_cluster::CacheRoutingMode;
use super::cache_persistence::{CacheDurabilityMode, DurableCacheStore};
use super::resp_cache::{CacheCommandError, CacheCommandTarget};
use super::cache_dispatch::{
    CacheDispatchConfigError, CacheDispatchWake, CacheDispatcher, CacheShardInbox,
};
use super::cache_pipeline::{CachePipelineError, CacheResponsePipeline};

const LISTENER_TOKEN: Token = Token(0);
const WAKE_TOKEN: Token = Token(1);
const FIRST_CONNECTION_TOKEN: usize = 2;

#[derive(Debug, Clone)]
pub struct CacheServerConfig {
    pub max_connections: usize,
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

pub enum CacheServerStore {
    Memory(CacheStore),
    Durable(DurableCacheStore),
}

impl CacheServerStore {
    fn cache_store(&self) -> &CacheStore {
        match self {
            Self::Memory(store) => store,
            Self::Durable(store) => store.store(),
        }
    }

    fn purge_expired(&mut self, now_ms: u64, max_items: usize) -> usize {
        match self {
            Self::Memory(store) => store.purge_expired(now_ms, max_items),
            Self::Durable(store) => store.purge_expired(now_ms, max_items),
        }
    }

    pub fn durability_mode(&self) -> CacheDurabilityMode {
        match self {
            Self::Memory(_) => CacheDurabilityMode::Memory,
            Self::Durable(store) => store.durability_mode(),
        }
    }

    fn memory_store_mut(&mut self) -> Option<&mut CacheStore> {
        match self {
            Self::Memory(store) => Some(store),
            Self::Durable(_) => None,
        }
    }
}

impl CacheCommandTarget for CacheServerStore {
    fn get<'a>(&'a mut self, key: &[u8], now_ms: u64) -> Option<CacheValueView<'a>> {
        match self {
            Self::Memory(store) => CacheCommandTarget::get(store, key, now_ms),
            Self::Durable(store) => CacheCommandTarget::get(store, key, now_ms),
        }
    }

    fn exists(&mut self, key: &[u8], now_ms: u64) -> bool {
        match self {
            Self::Memory(store) => CacheCommandTarget::exists(store, key, now_ms),
            Self::Durable(store) => CacheCommandTarget::exists(store, key, now_ms),
        }
    }

    fn set_bytes(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<(), CacheCommandError> {
        match self {
            Self::Memory(store) => CacheCommandTarget::set_bytes(store, key, value, ttl_ms, now_ms),
            Self::Durable(store) => CacheCommandTarget::set_bytes(store, key, value, ttl_ms, now_ms),
        }
    }

    fn set_many_bytes(
        &mut self,
        pairs: &[(&[u8], &[u8])],
        now_ms: u64,
    ) -> Result<(), CacheCommandError> {
        match self {
            Self::Memory(store) => CacheCommandTarget::set_many_bytes(store, pairs, now_ms),
            Self::Durable(store) => CacheCommandTarget::set_many_bytes(store, pairs, now_ms),
        }
    }

    fn delete_many(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<usize, CacheCommandError> {
        match self {
            Self::Memory(store) => CacheCommandTarget::delete_many(store, keys, now_ms),
            Self::Durable(store) => CacheCommandTarget::delete_many(store, keys, now_ms),
        }
    }

    fn increment(
        &mut self,
        key: &[u8],
        delta: i64,
        now_ms: u64,
    ) -> Result<i64, CacheCommandError> {
        match self {
            Self::Memory(store) => CacheCommandTarget::increment(store, key, delta, now_ms),
            Self::Durable(store) => CacheCommandTarget::increment(store, key, delta, now_ms),
        }
    }

    fn expire_ms(
        &mut self,
        key: &[u8],
        ttl_ms: u64,
        now_ms: u64,
    ) -> Result<bool, CacheCommandError> {
        match self {
            Self::Memory(store) => CacheCommandTarget::expire_ms(store, key, ttl_ms, now_ms),
            Self::Durable(store) => CacheCommandTarget::expire_ms(store, key, ttl_ms, now_ms),
        }
    }

    fn ttl(&mut self, key: &[u8], now_ms: u64) -> CacheTtl {
        match self {
            Self::Memory(store) => CacheCommandTarget::ttl(store, key, now_ms),
            Self::Durable(store) => CacheCommandTarget::ttl(store, key, now_ms),
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
    store: CacheServerStore,
    connections: HashMap<Token, CacheConnection>,
    next_connection_token: usize,
    config: CacheServerConfig,
    clock: CacheServerClock,
    shutdown: Arc<AtomicBool>,
    waker: Arc<Waker>,
    next_expiry_sweep: Instant,
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
        Self::bind_store(
            bind_addr,
            dispatcher,
            inbox,
            CacheServerStore::Memory(store),
            config,
            clock,
        )
    }

    pub fn bind_durable(
        bind_addr: SocketAddr,
        dispatcher: CacheDispatcher,
        inbox: CacheShardInbox,
        store: DurableCacheStore,
        config: CacheServerConfig,
        clock: CacheServerClock,
    ) -> Result<Self, CacheServerError> {
        Self::bind_store(
            bind_addr,
            dispatcher,
            inbox,
            CacheServerStore::Durable(store),
            config,
            clock,
        )
    }

    fn bind_store(
        bind_addr: SocketAddr,
        dispatcher: CacheDispatcher,
        inbox: CacheShardInbox,
        store: CacheServerStore,
        config: CacheServerConfig,
        clock: CacheServerClock,
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
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn control(&self) -> CacheShardServerControl {
        CacheShardServerControl {
            shutdown: self.shutdown.clone(),
            waker: self.waker.clone(),
        }
    }

    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    pub fn store(&self) -> &CacheStore {
        self.store.cache_store()
    }

    /// Mutable access is intentionally available only for Memory mode.
    ///
    /// Returning None for journaled modes prevents callers from bypassing the
    /// durability wrapper and mutating CacheStore without a WAL record.
    pub fn memory_store_mut(&mut self) -> Option<&mut CacheStore> {
        self.store.memory_store_mut()
    }

    pub fn durability_mode(&self) -> CacheDurabilityMode {
        self.store.durability_mode()
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
        self.inbox
            .drain(&mut self.store, self.config.max_inbox_batch);
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
    use super::super::cache_persistence::{recover_cache, CacheWal};
    use super::super::cache_routing::CacheSlotMap;
    use super::*;
    use std::net::TcpStream as StdTcpStream;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_CACHE_SERVER_TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn test_path(name: &str) -> std::path::PathBuf {
        let id = NEXT_CACHE_SERVER_TEST_ID.fetch_add(1, AtomicOrdering::Relaxed);
        std::env::temp_dir().join(format!(
            "nulang-cache-server-{name}-{}-{id}",
            std::process::id()
        ))
    }

    fn wall_now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
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
    fn durable_reactor_journals_real_resp_mutation_and_recovers_it() {
        let wal_path = test_path("durable-wal");
        let snapshot_path = test_path("durable-snapshot");

        let placement = CacheSlotMap::new_local(1, 1).unwrap();
        let owner = placement.owner_for_slot(0).unwrap();
        let (channels, mut inboxes) = CacheDispatchChannels::new(1, 32).unwrap();
        let mut endpoints = CacheEndpointMap::new();
        endpoints.insert(owner, CacheAdvertisedEndpoint::new("127.0.0.1", 7000));
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels)
            .unwrap()
            .with_cluster_redirects(endpoints);

        let wal = CacheWal::create_after(&wal_path, 0).unwrap();
        let durable = DurableCacheStore::with_wal(
            CacheStore::new(),
            wal,
            CacheDurabilityMode::SyncedJournal,
        )
        .unwrap();
        let clock = CacheServerClock::new();
        let mut server = CacheShardServer::bind_durable(
            "127.0.0.1:0".parse().unwrap(),
            dispatcher,
            inboxes.remove(0),
            durable,
            CacheServerConfig::default(),
            clock,
        )
        .unwrap();

        let address = server.local_addr().unwrap();
        let mut client = StdTcpStream::connect(address).unwrap();
        client.set_nodelay(true).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client
            .write_all(b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n")
            .unwrap();

        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(10))).unwrap();
        }
        let mut response = [0u8; 5];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"+OK\r\n");
        drop(client);
        drop(server);

        let (mut recovered, report) = recover_cache(
            &snapshot_path,
            &wal_path,
            CacheConfig::default(),
            CacheEvictionPolicy::S3Fifo,
            0,
            wall_now_ms(),
        )
        .unwrap();
        assert_eq!(report.snapshot_sequence, 0);
        assert_eq!(report.wal_last_sequence, 1);
        assert_eq!(report.replayed_records, 1);
        assert_eq!(
            recovered.get(b"key", 0),
            Some(CacheValueView::Bytes(b"value"))
        );

        let _ = std::fs::remove_file(wal_path);
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
        server.memory_store_mut().unwrap().set_bytes(b"ttl", b"value", Some(1), now);

        std::thread::sleep(Duration::from_millis(3));
        server.poll_once(Some(Duration::from_millis(1))).unwrap();

        let now = server.clock.now_ms();
        assert!(server.memory_store_mut().unwrap().get(b"ttl", now).is_none());
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
