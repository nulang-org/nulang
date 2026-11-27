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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
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

const LISTENER_TOKEN: Token = Token(0);
const WAKE_TOKEN: Token = Token(1);
const FIRST_CONNECTION_TOKEN: usize = 2;

/// Shared limit on retained connection input/output vector capacity.
/// Clone this budget into multiple shard reactors to share one process allowance.
#[derive(Debug, Clone)]
pub struct CacheBufferBudget {
    inner: Arc<BufferBudgetInner>,
}
#[derive(Debug)]
struct BufferBudgetInner {
    limit: usize,
    used: AtomicUsize,
}
impl CacheBufferBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(BufferBudgetInner {
                limit,
                used: AtomicUsize::new(0),
            }),
        }
    }
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
    fn lease(&self) -> BufferLease {
        BufferLease {
            budget: self.clone(),
            charged: 0,
        }
    }
}
struct BufferLease {
    budget: CacheBufferBudget,
    charged: usize,
}
impl BufferLease {
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
    lease: BufferLease,
    writable_interest: bool,
}

impl CacheConnection {
    fn new(stream: TcpStream, lease: BufferLease) -> Self {
        Self {
            stream,
            input: Vec::new(),
            input_start: 0,
            output: Vec::new(),
            output_start: 0,
            lease,
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
    buffer_budget: CacheBufferBudget,
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
            poll,
            listener,
            dispatcher,
            inbox,
            store,
            connections: HashMap::new(),
            next_connection_token: FIRST_CONNECTION_TOKEN,
            config,
            buffer_budget,
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
                        CacheConnection::new(stream, self.buffer_budget.lease()),
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
                    // Compact consumed bytes before reserving the next extent.
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
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn process_input(&mut self, connection: &mut CacheConnection) -> Result<(), ConnectionAction> {
        loop {
            if connection.input_start == connection.input.len() {
                connection.compact_input();
                return Ok(());
            }
            let input = &connection.input[connection.input_start..];
            let Some((command, consumed)) =
                parse_command(input).map_err(|_| ConnectionAction::Close)?
            else {
                connection.compact_input();
                return Ok(());
            };
            let now = self.clock.now_ms();
            let bound = self
                .dispatcher
                .redirect_reply_bound(&self.store, command, now);
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
            let outcome = self
                .dispatcher
                .dispatch_frame(&mut self.store, input, now, &mut connection.output)
                .map_err(|_| ConnectionAction::Close)?;
            match outcome {
                Some(CacheDispatchOutcome::Executed { consumed: actual })
                | Some(CacheDispatchOutcome::Redirected {
                    consumed: actual, ..
                }) if actual == consumed => {}
                _ => return Err(ConnectionAction::Close),
            }
            debug_assert!(connection.output.len() <= required);
            connection.input_start += consumed;
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
    if config.max_total_connection_bytes == 0 {
        return Err(CacheServerError::InvalidConfig(
            "max_total_connection_bytes must be non-zero",
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
            .set_bytes(b"ttl", b"value", Some(1), now)
            .unwrap();

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
        assert!(server.store_mut().exists(b"k", 0));
    }

    #[test]
    fn network_clients_share_budget_and_disconnect_releases_capacity() {
        let mut server = build_server();
        server.config.max_input_buffer = 256;
        server.config.max_output_buffer = 256;
        server.buffer_budget = CacheBufferBudget::new(512);
        let addr = server.local_addr().unwrap();
        let ping = b"*1\r\n$4\r\nPING\r\n";
        let mut first = StdTcpStream::connect(addr).unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        first.write_all(ping).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        let mut response = [0; 7];
        first.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"+PONG\r\n");
        assert_eq!(server.buffer_budget.used_bytes(), 512);

        let mut second = StdTcpStream::connect(addr).unwrap();
        second.write_all(ping).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(server.connection_count(), 1);
        assert_eq!(server.buffer_budget.used_bytes(), 512);
        drop(first);
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.buffer_budget.used_bytes(), 0);
        let mut third = StdTcpStream::connect(addr).unwrap();
        third
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        third.write_all(&ping[..5]).unwrap();
        for _ in 0..4 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        third.write_all(&ping[5..]).unwrap();
        for _ in 0..8 {
            server.poll_once(Some(Duration::from_millis(5))).unwrap();
        }
        third.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"+PONG\r\n");
    }
}
