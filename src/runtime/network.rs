//! Network transport layer for Nulang's distributed actor runtime.
//!
//! This module enables actors on different machines to send messages to each
//! other transparently over TCP. It defines a binary wire protocol, manages
//! connection pooling, and runs background threads for asynchronous I/O.
//!
//! # Architecture
//!
//! Each node runs a [`NetworkTransport`] that:
//! 1. Listens on a TCP socket for incoming connections from peer nodes.
//! 2. Maintains a pool of active [`TcpConnection`]s to remote nodes.
//! 3. Receives [`Packet`]s from peers and exposes them via [`receive`][NetworkTransport::receive].
//! 4. Sends [`Packet`]s to peers via an internal outgoing queue.
//!
//! # Wire Protocol
//!
//! Every packet on the wire is length-prefixed:
//! ```text
//! [0..4]   message length (u32, big-endian, includes this header)
//! [4..8]   magic: "NUL0"
//! [8]      packet type discriminant
//! [9..17]  sequence number (u64, big-endian)
//! [17..]   type-specific payload
//! ```
//!
//! A 16-byte versioned handshake is exchanged immediately after the TCP
//! connection is established, *before* either side starts sending framed
//! packets: `[magic "NUL0"][version u32][node_id u64]`. A peer whose wire
//! version does not match [`crate::format::constants::WIRE_VERSION`] is
//! refused, never silently reinterpreted. See `SPEC2.md` §"Format Stability".

use std::collections::{HashMap, HashSet};
use std::io;
#[cfg(feature = "tcp")]
use std::io::{Read, Write};
use std::net::SocketAddr;
#[cfg(feature = "tcp")]
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "tcp")]
use std::sync::Mutex;
use std::sync::{mpsc, Arc};
#[cfg(feature = "tcp")]
use std::thread::{self, JoinHandle};
#[cfg(feature = "tcp")]
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Imports from the rest of the crate
// ---------------------------------------------------------------------------

use super::cluster::{DurableDirectoryEntry, NodeGossip, NodeStatus};
use super::crdt_manager::{CrdtDeltaOp, CrdtOp};
use super::supervision::RemoteLink;
use super::MessagePriority;
use super::NodeId;
use crate::vm::Value;

#[cfg(feature = "tcp")]
use tracing::warn;

// ---------------------------------------------------------------------------
// TLS configuration
// ---------------------------------------------------------------------------

/// TLS configuration for the NUL0 wire protocol.
///
/// When `MutualTls` is active, every TCP connection is upgraded to TLS
/// immediately after TCP connect/accept but *before* the NUL0 versioned
/// handshake. Both sides present certificates signed by the same cluster
/// CA; each side verifies the peer's certificate against that CA, and node
/// identity is derived from the certificate fingerprint (BLAKE3) rather
/// than the spoofable socket-address hash.
///
/// `PlaintextInsecure` is the explicit opt-out for development and testing.
#[derive(Clone)]
#[cfg(feature = "tcp")]
pub enum TlsConfig {
    /// Mutual TLS with a cluster CA.
    ///
    /// Both server and client present certificates signed by the same CA.
    /// The CA certificate is used to verify the peer; the server certificate
    /// and key are presented to peers. Node identity is derived from the
    /// server certificate's DER fingerprint via BLAKE3.
    MutualTls {
        /// PEM-encoded CA certificate that signed both server and client certs.
        ca_cert_pem: Vec<u8>,
        /// PEM-encoded server certificate (presented to connecting peers).
        server_cert_pem: Vec<u8>,
        /// PEM-encoded server private key (RSA or ECDSA, PKCS#8 format).
        server_key_pem: Vec<u8>,
        /// Expected server name for TLS certificate verification.
        /// Defaults to `"localhost"` when `None`.
        server_name: Option<String>,
    },
    /// Plaintext with no encryption or authentication.
    ///
    /// Explicit opt-out. Node identity is derived from a hash of the bind
    /// address. Insecure; not recommended for production deployments.
    PlaintextInsecure,
}

#[cfg(feature = "tcp")]
impl TlsConfig {
    /// Returns `true` when plaintext (insecure) transport is configured.
    pub fn is_plaintext(&self) -> bool {
        matches!(self, TlsConfig::PlaintextInsecure)
    }

    /// Return the configured server name for TLS certificate verification.
    /// Returns `None` for variants that don't use mutual TLS.
    pub fn server_name(&self) -> Option<&str> {
        match self {
            TlsConfig::MutualTls { server_name, .. } => server_name.as_deref(),
            _ => None,
        }
    }

    /// Build a `rustls::ServerConfig` for accepting TLS connections.
    ///
    /// For `MutualTls`: configures the server certificate + key, requires
    /// client authentication, and verifies client certificates against the
    /// configured CA.
    fn server_config(&self) -> io::Result<rustls::ServerConfig> {
        match self {
            TlsConfig::MutualTls { .. } => {
                let (ca, cert, key) = self.mutual_tls_material()?;
                let ca_cert = parse_pem_cert(ca)?;
                let server_cert = parse_pem_cert_chain(cert)?;
                let server_key = parse_pem_key(key)?;
                let mut client_auth_roots = rustls::RootCertStore::empty();
                client_auth_roots.add(ca_cert).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad CA cert: {e}"))
                })?;
                let client_verifier = rustls::server::WebPkiClientVerifier::builder(
                    std::sync::Arc::new(client_auth_roots),
                )
                .build()
                .map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("client verifier: {e}"))
                })?;
                rustls::ServerConfig::builder()
                    .with_client_cert_verifier(client_verifier)
                    .with_single_cert(server_cert, server_key)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
            TlsConfig::PlaintextInsecure => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TlsConfig::PlaintextInsecure has no server config",
            )),
        }
    }

    fn client_config(&self) -> io::Result<rustls::ClientConfig> {
        match self {
            TlsConfig::MutualTls { .. } => {
                let (ca, cert, key) = self.mutual_tls_material()?;
                let ca_cert = parse_pem_cert(ca)?;
                let client_cert = parse_pem_cert_chain(cert)?;
                let client_key = parse_pem_key(key)?;
                let mut roots = rustls::RootCertStore::empty();
                roots.add(ca_cert).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad CA cert: {e}"))
                })?;
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_client_auth_cert(client_cert, client_key)
                    .map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("client config: {e}"))
                    })
            }
            TlsConfig::PlaintextInsecure => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TlsConfig::PlaintextInsecure has no client config",
            )),
        }
    }

    /// Extract the (ca_cert_pem, server_cert_pem, server_key_pem) triple
    /// for `MutualTls`, or return an error for other variants.
    fn mutual_tls_material(&self) -> io::Result<(&[u8], &[u8], &[u8])> {
        match self {
            TlsConfig::MutualTls {
                ca_cert_pem,
                server_cert_pem,
                server_key_pem,
                ..
            } => Ok((ca_cert_pem, server_cert_pem, server_key_pem)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a MutualTls config",
            )),
        }
    }

    /// The DER-encoded server certificate, for NodeId derivation.
    /// Returns `None` for `PlaintextInsecure`.
    pub fn server_cert_der(&self) -> Option<Vec<u8>> {
        match self {
            TlsConfig::MutualTls {
                server_cert_pem, ..
            } => {
                let certs = rustls_pemfile::certs(&mut server_cert_pem.as_slice())
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                certs.first().map(|c| c.clone().to_vec())
            }
            TlsConfig::PlaintextInsecure => None,
        }
    }
}

/// Parse a single PEM-encoded X.509 certificate.
#[cfg(feature = "tcp")]
fn parse_pem_cert(pem: &[u8]) -> io::Result<rustls::pki_types::CertificateDer<'static>> {
    let certs: Vec<rustls::pki_types::CertificateDer> =
        rustls_pemfile::certs(&mut std::io::BufReader::new(pem))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("PEM cert: {e}")))?;
    certs
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty PEM cert"))
}

/// Parse a chain of PEM-encoded X.509 certificates (at least one).
#[cfg(feature = "tcp")]
fn parse_pem_cert_chain(pem: &[u8]) -> io::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let certs: Vec<rustls::pki_types::CertificateDer> =
        rustls_pemfile::certs(&mut std::io::BufReader::new(pem))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("PEM cert chain: {e}"))
            })?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty PEM cert chain",
        ));
    }
    Ok(certs)
}
/// Parse a PEM-encoded private key (PKCS#8 or RSA).
#[cfg(feature = "tcp")]
fn parse_pem_key(pem: &[u8]) -> io::Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(pem))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("PEM key: {e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty PEM key"))?;
    Ok(key)
}

// ---------------------------------------------------------------------------
// TransportStream — abstracts over raw TCP and TLS-wrapped streams
// ---------------------------------------------------------------------------

/// A duplex transport stream that can be either a plain `TcpStream` or a
/// TLS-wrapped connection.  TLS streams are shared behind `Arc<Mutex<>>`
/// because `rustls::StreamOwned` cannot be cloned the way `TcpStream` can.
#[cfg(feature = "tcp")]
pub(crate) enum TransportStream {
    Raw(TcpStream),
    TlsServer(
        std::sync::Arc<std::sync::Mutex<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>>,
    ),
    TlsClient(
        std::sync::Arc<std::sync::Mutex<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>>,
    ),
}

#[cfg(feature = "tcp")]
impl TransportStream {
    /// Create a clone suitable for the reader half of a duplex connection.
    /// For raw TCP this duplicates the file descriptor; for TLS this clones
    /// the `Arc` so reader and writer share the same TLS session.
    fn try_clone(&self) -> io::Result<TransportStream> {
        match self {
            TransportStream::Raw(s) => Ok(TransportStream::Raw(s.try_clone()?)),
            TransportStream::TlsServer(s) => Ok(TransportStream::TlsServer(s.clone())),
            TransportStream::TlsClient(s) => Ok(TransportStream::TlsClient(s.clone())),
        }
    }

    /// Attempt a graceful TLS close (send `close_notify` + shut down the
    /// underlying TCP socket). If the TLS session lock is held by a reader
    /// thread blocked on I/O, skip the graceful close to avoid deadlock —
    /// the OS will clean up the socket when the process exits.
    fn shutdown(&self) -> io::Result<()> {
        match self {
            TransportStream::Raw(s) => s.shutdown(std::net::Shutdown::Both),
            TransportStream::TlsServer(s) => {
                if let Ok(mut locked) = s.try_lock() {
                    let _ = locked.conn.send_close_notify();
                    locked.get_ref().shutdown(std::net::Shutdown::Both)
                } else {
                    Ok(())
                }
            }
            TransportStream::TlsClient(s) => {
                if let Ok(mut locked) = s.try_lock() {
                    let _ = locked.conn.send_close_notify();
                    locked.get_ref().shutdown(std::net::Shutdown::Both)
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Set the underlying TCP stream's read timeout. For TLS streams, must
    /// acquire the session lock.
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            TransportStream::Raw(s) => s.set_read_timeout(timeout),
            TransportStream::TlsServer(s) => {
                let locked = s.lock().unwrap();
                locked.get_ref().set_read_timeout(timeout)
            }
            TransportStream::TlsClient(s) => {
                let locked = s.lock().unwrap();
                locked.get_ref().set_read_timeout(timeout)
            }
        }
    }

    /// Return the peer's certificate fingerprint as a `NodeId`, if TLS is
    /// active and the peer presented a certificate.
    ///
    /// Used to verify that the NUL0 handshake's claimed `node_id` matches
    /// the cryptographic identity established by the TLS session.
    fn peer_cert_node_id(&self) -> Option<NodeId> {
        let certs: Option<Vec<rustls::pki_types::CertificateDer>> = match self {
            TransportStream::TlsServer(s) => {
                let locked = s.lock().unwrap();
                locked.conn.peer_certificates().map(|c| c.to_vec())
            }
            TransportStream::TlsClient(s) => {
                let locked = s.lock().unwrap();
                locked.conn.peer_certificates().map(|c| c.to_vec())
            }
            TransportStream::Raw(_) => return None,
        };
        certs
            .and_then(|mut c| {
                if c.is_empty() {
                    None
                } else {
                    Some(c.swap_remove(0))
                }
            })
            .map(|cert| NodeId::from_cert_der(&cert))
    }
}

#[cfg(feature = "tcp")]
impl Read for TransportStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TransportStream::Raw(s) => s.read(buf),
            TransportStream::TlsServer(s) => s.lock().unwrap().read(buf),
            TransportStream::TlsClient(s) => s.lock().unwrap().read(buf),
        }
    }
}

#[cfg(feature = "tcp")]
impl Write for TransportStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TransportStream::Raw(s) => s.write(buf),
            TransportStream::TlsServer(s) => s.lock().unwrap().write(buf),
            TransportStream::TlsClient(s) => s.lock().unwrap().write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            TransportStream::Raw(s) => s.flush(),
            TransportStream::TlsServer(s) => s.lock().unwrap().flush(),
            TransportStream::TlsClient(s) => s.lock().unwrap().flush(),
        }
    }
}

#[cfg(feature = "tcp")]
fn tls_wrap_server(tcp: TcpStream, config: &TlsConfig) -> io::Result<TransportStream> {
    let cfg = config.server_config()?;
    let conn = rustls::ServerConnection::new(std::sync::Arc::new(cfg))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    // Read timeout is set after the handshake in connection_reader/connect.
    Ok(TransportStream::TlsServer(std::sync::Arc::new(
        std::sync::Mutex::new(rustls::StreamOwned::new(conn, tcp)),
    )))
}

#[cfg(feature = "tcp")]
fn tls_wrap_client(tcp: TcpStream, config: &TlsConfig) -> io::Result<TransportStream> {
    let cfg = config.client_config()?;
    let name_str: String = config.server_name().unwrap_or("localhost").to_owned();
    let name = rustls::pki_types::ServerName::try_from(name_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let conn = rustls::ClientConnection::new(std::sync::Arc::new(cfg), name)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    // Read timeout is set after the handshake in connection_reader/connect.
    Ok(TransportStream::TlsClient(std::sync::Arc::new(
        std::sync::Mutex::new(rustls::StreamOwned::new(conn, tcp)),
    )))
}

// ---------------------------------------------------------------------------
// TransportAddr — network address for TCP or Unix domain sockets
// ---------------------------------------------------------------------------

/// Address for the NUL0 protocol.  TCP is the default; Unix domain sockets
/// enable same-host eBPF sockmap redirection in NLC deployments.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TransportAddr {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(std::path::PathBuf),
}

impl TransportAddr {
    pub fn tcp(addr: SocketAddr) -> Self {
        TransportAddr::Tcp(addr)
    }
    #[cfg(unix)]
    pub fn unix(path: impl Into<std::path::PathBuf>) -> Self {
        TransportAddr::Unix(path.into())
    }
}

impl std::fmt::Display for TransportAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportAddr::Tcp(a) => write!(f, "{}", a),
            #[cfg(unix)]
            TransportAddr::Unix(p) => write!(f, "unix:{}", p.display()),
        }
    }
}
// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic bytes that prefix every packet payload (after the length header).
/// The single source of truth is [`crate::format::constants::WIRE_MAGIC`];
/// this re-exports it for the packet framer.
const MAGIC: &[u8] = &crate::format::constants::WIRE_MAGIC;

/// Total size of the fixed packet header: 4 magic + 1 type + 8 seq.
const PACKET_HEADER_LEN: usize = 13;

/// TCP read / write timeout applied to every connection.
#[cfg(feature = "tcp")]
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the sender thread waits on the outgoing channel before
/// re-checking the shutdown flag.
#[cfg(feature = "tcp")]
const CHANNEL_RECV_TIMEOUT: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Versioned handshake helpers (magic + version + node_id = 16 bytes)
// ---------------------------------------------------------------------------

/// Write the 16-byte NUL0 versioned handshake to a stream.
#[cfg(feature = "tcp")]
fn write_handshake<W: Write>(w: &mut W, node_id: NodeId) -> io::Result<()> {
    w.write_all(&crate::format::constants::WIRE_MAGIC)?;
    w.write_all(&crate::format::constants::WIRE_VERSION.to_be_bytes())?;
    w.write_all(&node_id.0.to_be_bytes())?;
    w.flush()
}

/// Read the 16-byte NUL0 versioned handshake from a stream, validating the
/// magic and the wire protocol version. Returns the peer's node id. A
/// mismatched magic or version is a hard error: the connection is refused
/// rather than the peer's packets being reinterpreted under the wrong layout.
#[cfg(feature = "tcp")]
fn read_handshake<R: Read>(r: &mut R) -> io::Result<NodeId> {
    let mut buf = [0u8; crate::format::constants::WIRE_HANDSHAKE_LEN];
    r.read_exact(&mut buf)?;
    if &buf[0..4] != crate::format::constants::WIRE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "wire handshake: bad magic, expected {:?}, got {:?}",
                crate::format::constants::WIRE_MAGIC,
                &buf[0..4]
            ),
        ));
    }
    let version = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if version != crate::format::constants::WIRE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "wire handshake: peer speaks wire version {version}, this runtime speaks {}",
                crate::format::constants::WIRE_VERSION
            ),
        ));
    }
    let node_id = NodeId(u64::from_be_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]));
    Ok(node_id)
}

/// Maximum length (in bytes) of a single packet payload we are willing to
/// deserialize — a simple DoS protection.
#[cfg(feature = "tcp")]
const MAX_PACKET_LEN: u32 = 16 * 1024 * 1024; // 16 MiB

/// Capacity of the bounded internal channels.
const CHANNEL_CAPACITY: usize = 1024;

// Packet type discriminants.
const TYPE_ACTOR_MESSAGE: u8 = 0;
const TYPE_HEARTBEAT: u8 = 1;
const TYPE_ACK: u8 = 2;
const TYPE_SPAWN_REQUEST: u8 = 3;
const TYPE_SPAWN_RESPONSE: u8 = 4;
const TYPE_CRDT_SYNC: u8 = 5;
const TYPE_GOSSIP: u8 = 6;
const TYPE_CRDT_DELTA_SYNC: u8 = 7;
const TYPE_FETCH_BEHAVIOR_REQUEST: u8 = 8;
const TYPE_FETCH_BEHAVIOR_RESPONSE: u8 = 9;
const TYPE_LINK: u8 = 10;
const TYPE_MONITOR: u8 = 11;
const TYPE_DOWN: u8 = 12;
const TYPE_CRDT_OP: u8 = 13;
const TYPE_MIGRATE_ACTOR: u8 = 14;
const TYPE_NODE_GOODBYE: u8 = 15;
const TYPE_SHADOW_REPLICATE: u8 = 16;

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

// NodeId is imported from super::cluster::NodeId

// ---------------------------------------------------------------------------
// Packet
// ---------------------------------------------------------------------------

/// A packet sent over the network between Nulang nodes.
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    /// Send a message to an actor on the target node.
    ///
    /// The behavior is identified by **name**, not by id: behavior ids are
    /// per-actor-table indices and are meaningless across nodes. The
    /// receiving node resolves the name against the target actor's behavior
    /// table (the same rule local sends use in `Runtime::behavior_id_for`).
    ActorMessage {
        target_actor: u64,
        behavior_name: String,
        /// Optional BLAKE3 content hash of the expected behavior implementation.
        /// Set by the sender if the behavior has a known content hash in the
        /// sender's module; the receiver MAY verify it against the local
        /// behavior table during delivery (see process_network_packets).
        content_hash: Option<[u8; 32]>,
        payload: Vec<Value>,
        /// UTF-8 content for every `Value::string(id)` in `payload`: on the
        /// wire a string-id value indexes **this table**, never the sender's
        /// or receiver's constant pool (a pool id is meaningless across
        /// nodes). The sending runtime populates the table from the sender's
        /// module pool (`distributed::resolve_wire_strings`); the receiving
        /// runtime interns each entry into the target actor's module pool
        /// (`distributed::intern_wire_strings`).
        string_table: Vec<String>,
        /// Immutable byte payloads for every `Value::object(id)` in `payload`.
        /// On the wire an object-id value indexes **this table**.  The receiving
        /// runtime inserts each entry into its local `ObjectStore` and rewrites
        /// the payload to use the local object id before delivery.
        object_table: Vec<(u64, Vec<u8>)>,
        sender_actor: u64,
        sender_node: NodeId,
        priority: MessagePriority,
        /// Optional trace-id carried across nodes so a span begun on the
        /// sending node can continue on the receiving node (SPEC2 §15.3).
        trace_id: Option<String>,
    },

    /// Heartbeat / ping between nodes.
    Heartbeat {
        node_id: NodeId,
        timestamp: u64, // millis since epoch
    },

    /// Acknowledge receipt of a packet.
    Ack { packet_seq: u64 },

    /// Request to spawn an actor remotely.
    SpawnRequest {
        request_id: u64,
        behavior_name: String,
        /// Optional BLAKE3 content hash for cross-node behavior identity
        /// verification. The receiver MAY check this against the local
        /// `spawnable_behaviors` entry.
        content_hash: Option<[u8; 32]>,
        initial_state: Vec<(String, Value)>,
        bytecode: Option<Vec<u8>>,
    },

    /// Response to a spawn request.
    SpawnResponse {
        request_id: u64,
        actor_id: u64,
        success: bool,
    },

    /// CRDT synchronization packet.
    CrdtSync { ops: Arc<Vec<CrdtOp>> },

    /// Delta-state CRDT synchronization packet.
    ///
    /// Each op is tagged as a delta (changes since the sender's last sync)
    /// or a full-state snapshot — see [`CrdtDeltaOp`]. Receivers merge
    /// deltas into entries they already hold and apply full-state ops like
    /// [`CrdtSync`](Packet::CrdtSync). The full-state `CrdtSync` packet
    /// remains available as the join/reset fallback.
    CrdtDeltaSync { ops: Arc<Vec<CrdtDeltaOp>> },
    /// Low-bandwidth op-based CRDT replication: ships individual operations
    /// (e.g. "increment GCounter #5 by 1") rather than full or delta state.
    /// Full-state [`CrdtSync`](Packet::CrdtSync) and delta [`CrdtDeltaSync`](Packet::CrdtDeltaSync)
    /// remain as the join/repair fallback.
    CrdtOp { op: CrdtOp },

    /// Cluster membership gossip.
    ///
    /// Carries the sender's (compact) membership view; the receiver merges
    /// it via [`ClusterState::merge_membership`](crate::runtime::cluster::ClusterState::merge_membership),
    /// where higher incarnation numbers win. This is what gives membership
    /// transitive propagation: a node relays what it knows, so a chain of
    /// pairwise seeds still converges to a full mesh.
    Gossip {
        members: Vec<NodeGossip>,
        /// Durable-actor location directory entries (RFC 0014 §2),
        /// piggybacked on the membership gossip round. Additive: older
        /// peers that predate this field ignore the trailing bytes.
        directory: Vec<DurableDirectoryEntry>,
    },

    /// Request bytecode for a behavior identified by its BLAKE3 content hash.
    ///
    /// Sent by a node that receives a message for a behavior it doesn't have.
    /// The sender replies with `FetchBehaviorResponse` containing the compiled
    /// bytecode (as an NBC blob — see `src/format/nbc.rs`).
    FetchBehaviorRequest {
        /// The BLAKE3 content hash of the behavior being requested.
        content_hash: [u8; 32],
    },

    /// Response to a `FetchBehaviorRequest`, carrying the compiled bytecode.
    FetchBehaviorResponse {
        /// Echoes the content hash from the request for correlation.
        content_hash: [u8; 32],
        /// Behavior name (for the receiver's behavior table).
        behavior_name: String,
        /// Compiled NBC bytecode blob. `None` if the requested behavior
        /// is not known to the responding node.
        nbc_bytes: Option<Vec<u8>>,
    },
    /// Register a link between a local watcher and a remote target.
    Link {
        watcher: RemoteLink,
        target: RemoteLink,
    },
    /// Register a monitor between a local watcher and a remote target.
    Monitor {
        watcher: RemoteLink,
        target: RemoteLink,
    },
    /// Notify that an actor has exited (propagation of `DOWN`).
    Down { target: RemoteLink, reason: String },
    /// Migrate an actor to a different node.
    ///
    /// Carries the actor's durable state snapshot plus its NBC-encoded
    /// bytecode module so the target node can reconstruct and resume the
    /// actor without a shared persistence store.
    MigrateActor {
        actor_id: u64,
        /// NBC-encoded bytecode module (behaviors, metadata, constants).
        nbc_bytes: Vec<u8>,
        /// JSON-serialized [`ActorSnapshot`](crate::runtime::persistence::ActorSnapshot).
        snapshot_json: Vec<u8>,
    },
    /// A positive "goodbye" from a node that is shutting down (RFC 0014 §1
    /// path 1): the sender declares its durable re-spawn-opted actors
    /// checkpointed and terminated, so receivers may mark it confirmed-gone
    /// and re-spawn immediately. Carries `(actor_id, epoch)` pairs.
    NodeGoodbye {
        node_id: NodeId,
        durable: Vec<(u64, u64)>,
    },
    /// A shadow replica of a durable actor's snapshot (RFC 0014 §3),
    /// checkpointed from the home node to its deterministic shadow. Stored,
    /// not instantiated — the shadow re-spawns from it only when the home
    /// node is confirmed removed.
    ShadowReplicate {
        actor_id: u64,
        nbc_bytes: Vec<u8>,
        snapshot_json: Vec<u8>,
        epoch: u64,
    },
}

impl Packet {
    // ------------------------------------------------------------------
    // Public serialization API
    // ------------------------------------------------------------------

    /// Serialize the packet into bytes **without** the outer length prefix.
    ///
    /// The returned vector starts with [`MAGIC`], followed by the type
    /// discriminant, sequence number, and type-specific payload.
    pub fn to_bytes(&self, seq: u64) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);

        // Magic
        buf.extend_from_slice(MAGIC);

        // Type discriminant
        buf.push(self.discriminant());

        // Sequence number (big-endian)
        buf.extend_from_slice(&seq.to_be_bytes());

        // Payload
        self.write_payload(&mut buf);

        buf
    }

    /// Deserialize a packet from bytes (starting at the magic bytes).
    ///
    /// Returns `None` if the bytes are malformed or the discriminant is
    /// unknown.
    /// Deserialize a packet from bytes (starting at the magic bytes).
    ///
    /// Returns `None` if the bytes are malformed or the discriminant is
    /// unknown.
    pub fn from_bytes(bytes: &[u8]) -> Option<(u64, Self)> {
        if bytes.len() < PACKET_HEADER_LEN {
            return None;
        }
        if &bytes[0..4] != MAGIC {
            return None;
        }

        let discriminant = bytes[4];
        let seq = u64::from_be_bytes([
            bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11], bytes[12],
        ]);
        let payload = &bytes[PACKET_HEADER_LEN..];
        let packet = match discriminant {
            TYPE_ACTOR_MESSAGE => Self::read_actor_message(payload)?,
            TYPE_HEARTBEAT => Self::read_heartbeat(payload)?,
            TYPE_MIGRATE_ACTOR => Self::read_migrate_actor(payload)?,
            TYPE_ACK => Self::read_ack(payload)?,
            TYPE_SPAWN_REQUEST => Self::read_spawn_request(payload)?,
            TYPE_SPAWN_RESPONSE => Self::read_spawn_response(payload)?,
            TYPE_CRDT_SYNC => Self::read_crdt_sync(payload)?,
            TYPE_CRDT_DELTA_SYNC => Self::read_crdt_delta_sync(payload)?,
            TYPE_CRDT_OP => Self::read_crdt_op(payload)?,
            TYPE_GOSSIP => Self::read_gossip(payload)?,
            TYPE_FETCH_BEHAVIOR_REQUEST => Self::read_fetch_behavior_request(payload)?,
            TYPE_FETCH_BEHAVIOR_RESPONSE => Self::read_fetch_behavior_response(payload)?,
            TYPE_LINK => Self::read_link(payload)?,
            TYPE_MONITOR => Self::read_monitor(payload)?,
            TYPE_DOWN => Self::read_down(payload)?,
            TYPE_NODE_GOODBYE => Self::read_node_goodbye(payload)?,
            TYPE_SHADOW_REPLICATE => Self::read_shadow_replicate(payload)?,
            _ => return None,
        };
        Some((seq, packet))
    }

    fn read_link(payload: &[u8]) -> Option<Self> {
        let watcher_node = NodeId(u64::from_be_bytes(payload.get(0..8)?.try_into().ok()?));
        let watcher_actor = u64::from_be_bytes(payload.get(8..16)?.try_into().ok()?);
        let target_node = NodeId(u64::from_be_bytes(payload.get(16..24)?.try_into().ok()?));
        let target_actor = u64::from_be_bytes(payload.get(24..32)?.try_into().ok()?);
        Some(Packet::Link {
            watcher: RemoteLink {
                node_id: watcher_node,
                actor_id: watcher_actor,
            },
            target: RemoteLink {
                node_id: target_node,
                actor_id: target_actor,
            },
        })
    }

    fn read_monitor(payload: &[u8]) -> Option<Self> {
        let watcher_node = NodeId(u64::from_be_bytes(payload.get(0..8)?.try_into().ok()?));
        let watcher_actor = u64::from_be_bytes(payload.get(8..16)?.try_into().ok()?);
        let target_node = NodeId(u64::from_be_bytes(payload.get(16..24)?.try_into().ok()?));
        let target_actor = u64::from_be_bytes(payload.get(24..32)?.try_into().ok()?);
        Some(Packet::Monitor {
            watcher: RemoteLink {
                node_id: watcher_node,
                actor_id: watcher_actor,
            },
            target: RemoteLink {
                node_id: target_node,
                actor_id: target_actor,
            },
        })
    }

    fn read_down(payload: &[u8]) -> Option<Self> {
        let target_node = NodeId(u64::from_be_bytes(payload.get(0..8)?.try_into().ok()?));
        let target_actor = u64::from_be_bytes(payload.get(8..16)?.try_into().ok()?);
        let (reason, _) = read_string(payload, 16)?;
        Some(Packet::Down {
            target: RemoteLink {
                node_id: target_node,
                actor_id: target_actor,
            },
            reason,
        })
    }

    fn read_migrate_actor(payload: &[u8]) -> Option<Self> {
        if payload.len() < 12 {
            return None;
        }
        let actor_id = u64::from_be_bytes(payload[0..8].try_into().ok()?);
        let nbc_len = u32::from_be_bytes(payload[8..12].try_into().ok()?) as usize;
        if payload.len() < 12 + nbc_len + 4 {
            return None;
        }
        let nbc_bytes = payload[12..12 + nbc_len].to_vec();
        let json_off = 12 + nbc_len;
        let json_len =
            u32::from_be_bytes(payload[json_off..json_off + 4].try_into().ok()?) as usize;
        if payload.len() < json_off + 4 + json_len {
            return None;
        }
        let snapshot_json = payload[json_off + 4..json_off + 4 + json_len].to_vec();
        Some(Packet::MigrateActor {
            actor_id,
            nbc_bytes,
            snapshot_json,
        })
    }

    fn read_crdt_op(payload: &[u8]) -> Option<Self> {
        CrdtOp::from_bytes(payload).map(|op| Packet::CrdtOp { op })
    }

    // ------------------------------------------------------------------
    // Internal helpers
    fn discriminant(&self) -> u8 {
        match self {
            Packet::ActorMessage { .. } => TYPE_ACTOR_MESSAGE,
            Packet::Heartbeat { .. } => TYPE_HEARTBEAT,
            Packet::Ack { .. } => TYPE_ACK,
            Packet::SpawnRequest { .. } => TYPE_SPAWN_REQUEST,
            Packet::SpawnResponse { .. } => TYPE_SPAWN_RESPONSE,
            Packet::CrdtSync { .. } => TYPE_CRDT_SYNC,
            Packet::CrdtDeltaSync { .. } => TYPE_CRDT_DELTA_SYNC,
            Packet::CrdtOp { .. } => TYPE_CRDT_OP,
            Packet::Gossip { .. } => TYPE_GOSSIP,
            Packet::FetchBehaviorRequest { .. } => TYPE_FETCH_BEHAVIOR_REQUEST,
            Packet::FetchBehaviorResponse { .. } => TYPE_FETCH_BEHAVIOR_RESPONSE,
            Packet::Link { .. } => TYPE_LINK,
            Packet::Monitor { .. } => TYPE_MONITOR,
            Packet::Down { .. } => TYPE_DOWN,
            Packet::MigrateActor { .. } => TYPE_MIGRATE_ACTOR,
            Packet::NodeGoodbye { .. } => TYPE_NODE_GOODBYE,
            Packet::ShadowReplicate { .. } => TYPE_SHADOW_REPLICATE,
        }
    }

    fn write_payload(&self, buf: &mut Vec<u8>) {
        match self {
            Packet::ActorMessage {
                target_actor,
                behavior_name,
                content_hash,
                payload,
                string_table,
                object_table,
                sender_actor,
                sender_node,
                priority,
                trace_id,
            } => {
                buf.extend_from_slice(&target_actor.to_be_bytes());
                write_string(buf, behavior_name);
                // content_hash: 1 byte flag + optional 32 bytes
                write_optional_hash(buf, content_hash);
                buf.extend_from_slice(&sender_actor.to_be_bytes());
                buf.extend_from_slice(&sender_node.0.to_be_bytes());
                buf.push(*priority as u8);
                buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                for v in payload {
                    write_value(buf, v);
                }
                // String contents travel after the payload values; the
                // string-id values above index this table.
                buf.extend_from_slice(&(string_table.len() as u32).to_be_bytes());
                for s in string_table {
                    write_string(buf, s);
                }
                // Object payloads travel after the string table; the object-id
                // values above index this table.
                buf.extend_from_slice(&(object_table.len() as u32).to_be_bytes());
                for (id, bytes) in object_table {
                    buf.extend_from_slice(&id.to_be_bytes());
                    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                    buf.extend_from_slice(bytes);
                }
                // trace_id: 1-byte flag + optional content (string).
                match trace_id {
                    Some(tid) => {
                        buf.push(1);
                        write_string(buf, tid);
                    }
                    None => buf.push(0),
                }
            }
            Packet::Heartbeat { node_id, timestamp } => {
                buf.extend_from_slice(&node_id.0.to_be_bytes());
                buf.extend_from_slice(&timestamp.to_be_bytes());
            }
            Packet::Ack { packet_seq } => {
                buf.extend_from_slice(&packet_seq.to_be_bytes());
            }
            Packet::SpawnRequest {
                request_id,
                behavior_name,
                content_hash,
                initial_state,
                bytecode,
            } => {
                buf.extend_from_slice(&request_id.to_be_bytes());
                write_string(buf, behavior_name);
                write_optional_hash(buf, content_hash);
                buf.extend_from_slice(&(initial_state.len() as u32).to_be_bytes());
                for (k, v) in initial_state {
                    write_string(buf, k);
                    write_value(buf, v);
                }
                match bytecode {
                    Some(bytes) => {
                        buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                        buf.extend_from_slice(bytes);
                    }
                    None => {
                        buf.extend_from_slice(&0u32.to_be_bytes());
                    }
                }
            }
            Packet::SpawnResponse {
                request_id,
                actor_id,
                success,
            } => {
                buf.extend_from_slice(&request_id.to_be_bytes());
                buf.extend_from_slice(&actor_id.to_be_bytes());
                buf.push(if *success { 1 } else { 0 });
            }
            Packet::CrdtSync { ops } => {
                buf.extend_from_slice(&(ops.len() as u32).to_be_bytes());
                for op in ops.iter() {
                    buf.extend_from_slice(&op.to_bytes());
                }
            }
            Packet::CrdtDeltaSync { ops } => {
                buf.extend_from_slice(&(ops.len() as u32).to_be_bytes());
                for op in ops.iter() {
                    buf.extend_from_slice(&op.to_bytes());
                }
            }
            Packet::CrdtOp { op } => {
                buf.extend_from_slice(&op.to_bytes());
            }
            Packet::Gossip { members, directory } => {
                buf.extend_from_slice(&(members.len() as u32).to_be_bytes());
                for m in members {
                    buf.extend_from_slice(&m.node_id.0.to_be_bytes());
                    write_addr(buf, &m.address);
                    buf.push(status_to_u8(m.status));
                    buf.extend_from_slice(&m.incarnation.to_be_bytes());
                }
                // Directory entries ride AFTER the members so an old peer
                // (which predates the field) reads the members and simply
                // ignores the trailing bytes.
                buf.extend_from_slice(&(directory.len() as u32).to_be_bytes());
                for e in directory {
                    buf.extend_from_slice(&e.actor_id.to_be_bytes());
                    buf.extend_from_slice(&e.node_id.0.to_be_bytes());
                    buf.extend_from_slice(&e.epoch.to_be_bytes());
                }
            }
            Packet::FetchBehaviorRequest { content_hash } => {
                buf.extend_from_slice(content_hash);
            }
            Packet::FetchBehaviorResponse {
                content_hash,
                behavior_name,
                nbc_bytes,
            } => {
                buf.extend_from_slice(content_hash);
                write_string(buf, behavior_name);
                match nbc_bytes {
                    Some(bytes) => {
                        buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                        buf.extend_from_slice(bytes);
                    }
                    None => {
                        buf.extend_from_slice(&0u32.to_be_bytes());
                    }
                }
            }
            Packet::Link { watcher, target } => {
                buf.extend_from_slice(&watcher.node_id.0.to_be_bytes());
                buf.extend_from_slice(&watcher.actor_id.to_be_bytes());
                buf.extend_from_slice(&target.node_id.0.to_be_bytes());
                buf.extend_from_slice(&target.actor_id.to_be_bytes());
            }
            Packet::Monitor { watcher, target } => {
                buf.extend_from_slice(&watcher.node_id.0.to_be_bytes());
                buf.extend_from_slice(&watcher.actor_id.to_be_bytes());
                buf.extend_from_slice(&target.node_id.0.to_be_bytes());
                buf.extend_from_slice(&target.actor_id.to_be_bytes());
            }
            Packet::Down { target, reason } => {
                buf.extend_from_slice(&target.node_id.0.to_be_bytes());
                buf.extend_from_slice(&target.actor_id.to_be_bytes());
                write_string(buf, reason);
            }
            Packet::MigrateActor {
                actor_id,
                nbc_bytes,
                snapshot_json,
            } => {
                buf.extend_from_slice(&actor_id.to_be_bytes());
                buf.extend_from_slice(&(nbc_bytes.len() as u32).to_be_bytes());
                buf.extend_from_slice(nbc_bytes);
                buf.extend_from_slice(&(snapshot_json.len() as u32).to_be_bytes());
                buf.extend_from_slice(snapshot_json);
            }
            Packet::NodeGoodbye { node_id, durable } => {
                buf.extend_from_slice(&node_id.0.to_be_bytes());
                buf.extend_from_slice(&(durable.len() as u32).to_be_bytes());
                for (actor_id, epoch) in durable {
                    buf.extend_from_slice(&actor_id.to_be_bytes());
                    buf.extend_from_slice(&epoch.to_be_bytes());
                }
            }
            Packet::ShadowReplicate {
                actor_id,
                nbc_bytes,
                snapshot_json,
                epoch,
            } => {
                buf.extend_from_slice(&actor_id.to_be_bytes());
                buf.extend_from_slice(&(nbc_bytes.len() as u32).to_be_bytes());
                buf.extend_from_slice(nbc_bytes);
                buf.extend_from_slice(&(snapshot_json.len() as u32).to_be_bytes());
                buf.extend_from_slice(snapshot_json);
                buf.extend_from_slice(&epoch.to_be_bytes());
            }
        }
    }

    // --- Deserialisation helpers for each variant ---------------------

    fn read_actor_message(payload: &[u8]) -> Option<Self> {
        if payload.len() < 12 {
            return None;
        }
        let target_actor = read_u64(payload, 0)?;
        let (behavior_name, name_len) = read_string(payload, 8)?;
        let mut offset = 8usize.checked_add(name_len)?;
        // content_hash: 1 byte flag + optional 32 bytes
        let (content_hash, hash_consumed) = read_optional_hash(payload, offset)?;
        offset = offset.checked_add(hash_consumed)?;
        if payload.len() < offset + 21 {
            return None;
        }
        let sender_actor = read_u64(payload, offset)?;
        let sender_node = NodeId(read_u64(payload, offset + 8)?);
        let priority = match payload.get(offset + 16).copied()? {
            0 => MessagePriority::System,
            1 => MessagePriority::Normal,
            2 => MessagePriority::Bulk,
            _ => return None,
        };
        let count = read_u32(payload, offset + 17)? as usize;
        offset = offset.checked_add(21)?;
        let mut values = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let (v, consumed) = read_value(payload, offset)?;
            values.push(v);
            offset = offset.checked_add(consumed)?;
            if offset > payload.len() {
                return None;
            }
        }
        // String table: contents for the payload's string-id values.
        let table_count = read_u32(payload, offset)? as usize;
        offset = offset.checked_add(4)?;
        let mut string_table = Vec::with_capacity(table_count.min(1024));
        for _ in 0..table_count {
            let (s, consumed) = read_string(payload, offset)?;
            string_table.push(s);
            offset = offset.checked_add(consumed)?;
        }
        // Object table: immutable byte payloads for the payload's object-id values.
        let object_count = read_u32(payload, offset)? as usize;
        offset = offset.checked_add(4)?;
        let mut object_table = Vec::with_capacity(object_count.min(1024));
        for _ in 0..object_count {
            let id = read_u64(payload, offset)?;
            offset = offset.checked_add(8)?;
            let bytes_len = read_u32(payload, offset)? as usize;
            offset = offset.checked_add(4)?;
            if offset + bytes_len > payload.len() {
                return None;
            }
            let bytes = payload[offset..offset + bytes_len].to_vec();
            offset = offset.checked_add(bytes_len)?;
            object_table.push((id, bytes));
        }
        // trace_id: 1-byte flag + optional string content.
        let trace_id = if offset < payload.len() && payload[offset] == 1 {
            let _ = offset.checked_add(1)?;
            let (tid, consumed) = read_string(payload, offset + 1)?;
            let _ = offset.checked_add(consumed + 1)?;
            Some(tid)
        } else {
            let _ = offset.checked_add(1)?;
            None
        };
        Some(Packet::ActorMessage {
            target_actor,
            behavior_name,
            content_hash,
            payload: values,
            string_table,
            object_table,
            sender_actor,
            sender_node,
            priority,
            trace_id,
        })
    }

    fn read_heartbeat(payload: &[u8]) -> Option<Self> {
        if payload.len() < 16 {
            return None;
        }
        let node_id = NodeId(read_u64(payload, 0)?);
        let timestamp = read_u64(payload, 8)?;
        Some(Packet::Heartbeat { node_id, timestamp })
    }

    fn read_ack(payload: &[u8]) -> Option<Self> {
        let packet_seq = read_u64(payload, 0)?;
        Some(Packet::Ack { packet_seq })
    }

    fn read_spawn_request(payload: &[u8]) -> Option<Self> {
        if payload.len() < 8 {
            return None;
        }
        let request_id = read_u64(payload, 0)?;
        let (behavior_name, consumed) = read_string(payload, 8)?;
        let mut offset = 8 + consumed;
        // content_hash: 1 byte flag + optional 32 bytes
        let (content_hash, hash_consumed) = read_optional_hash(payload, offset)?;
        offset = offset.checked_add(hash_consumed)?;
        let count = read_u32(payload, offset)? as usize;
        offset += 4;
        let mut initial_state = Vec::with_capacity(count.min(256));
        for _ in 0..count {
            let (key, consumed_key) = read_string(payload, offset)?;
            offset = offset.checked_add(consumed_key)?;
            let (value, consumed_val) = read_value(payload, offset)?;
            offset = offset.checked_add(consumed_val)?;
            initial_state.push((key, value));
        }
        // Deserialize optional bytecode: 0 length = None.
        let bytecode_len = read_u32(payload, offset)? as usize;
        offset += 4;
        let bytecode = if bytecode_len > 0 {
            if offset + bytecode_len > payload.len() {
                return None;
            }
            Some(payload[offset..offset + bytecode_len].to_vec())
        } else {
            None
        };
        Some(Packet::SpawnRequest {
            request_id,
            behavior_name,
            content_hash,
            initial_state,
            bytecode,
        })
    }
    fn read_spawn_response(payload: &[u8]) -> Option<Self> {
        if payload.len() < 17 {
            return None;
        }
        let request_id = read_u64(payload, 0)?;
        let actor_id = read_u64(payload, 8)?;
        let success = payload.get(16).copied()? != 0;
        Some(Packet::SpawnResponse {
            request_id,
            actor_id,
            success,
        })
    }

    fn read_crdt_sync(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        let count = read_u32(payload, 0)? as usize;
        let mut offset = 4usize;
        let mut ops = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            if offset >= payload.len() {
                return None;
            }
            // Each CrdtOp: [id:u64][type:u8][len:u32][payload]
            if offset + 13 > payload.len() {
                return None;
            }
            // Parse id + type + len manually to compute op byte length
            let op_payload_len = u32::from_be_bytes([
                payload[offset + 9],
                payload[offset + 10],
                payload[offset + 11],
                payload[offset + 12],
            ]) as usize;
            let total_op_len = 13 + op_payload_len;
            if offset + total_op_len > payload.len() {
                return None;
            }
            let op = CrdtOp::from_bytes(&payload[offset..offset + total_op_len])?;
            offset += total_op_len;
            ops.push(op);
        }
        Some(Packet::CrdtSync { ops: Arc::new(ops) })
    }

    fn read_crdt_delta_sync(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        let count = read_u32(payload, 0)? as usize;
        let mut offset = 4usize;
        let mut ops = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            // Each CrdtDeltaOp: [is_delta:u8][id:u64][type:u8][len:u32][payload]
            if offset + 14 > payload.len() {
                return None;
            }
            // Parse flag + id + type + len manually to compute op byte length
            let op_payload_len = u32::from_be_bytes([
                payload[offset + 10],
                payload[offset + 11],
                payload[offset + 12],
                payload[offset + 13],
            ]) as usize;
            let total_op_len = 14 + op_payload_len;
            if offset + total_op_len > payload.len() {
                return None;
            }
            let op = CrdtDeltaOp::from_bytes(&payload[offset..offset + total_op_len])?;
            offset += total_op_len;
            ops.push(op);
        }
        Some(Packet::CrdtDeltaSync { ops: Arc::new(ops) })
    }

    fn read_gossip(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        let count = read_u32(payload, 0)? as usize;
        let mut offset = 4usize;
        let mut members = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            // Each entry: [node_id:u64][addr][status:u8][incarnation:u64]
            if offset + 8 > payload.len() {
                return None;
            }
            let node_id = NodeId(read_u64(payload, offset)?);
            offset += 8;
            let (address, consumed) = read_addr(payload, offset)?;
            offset = offset.checked_add(consumed)?;
            if offset + 9 > payload.len() {
                return None;
            }
            let status = status_from_u8(*payload.get(offset)?)?;
            offset += 1;
            let incarnation = read_u64(payload, offset)?;
            offset += 8;
            members.push(NodeGossip {
                node_id,
                address,
                status,
                incarnation,
            });
        }
        // Directory entries follow the members; an old peer stops reading
        // here, so a missing directory tail is treated as empty (additive).
        let mut directory = Vec::new();
        if offset + 4 <= payload.len() {
            let dcount = read_u32(payload, offset)? as usize;
            offset += 4;
            for _ in 0..dcount.min(4096) {
                if offset + 24 > payload.len() {
                    return None;
                }
                let actor_id = read_u64(payload, offset)?;
                let node_id = NodeId(read_u64(payload, offset + 8)?);
                let epoch = read_u64(payload, offset + 16)?;
                offset += 24;
                directory.push(DurableDirectoryEntry {
                    actor_id,
                    node_id,
                    epoch,
                });
            }
        }
        Some(Packet::Gossip { members, directory })
    }

    fn read_node_goodbye(payload: &[u8]) -> Option<Self> {
        if payload.len() < 12 {
            return None;
        }
        let node_id = NodeId(read_u64(payload, 0)?);
        let count = read_u32(payload, 8)? as usize;
        let mut offset = 12usize;
        let mut durable = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            if offset + 16 > payload.len() {
                return None;
            }
            let actor_id = read_u64(payload, offset)?;
            let epoch = read_u64(payload, offset + 8)?;
            offset += 16;
            durable.push((actor_id, epoch));
        }
        Some(Packet::NodeGoodbye { node_id, durable })
    }

    fn read_shadow_replicate(payload: &[u8]) -> Option<Self> {
        if payload.len() < 12 {
            return None;
        }
        let actor_id = read_u64(payload, 0)?;
        let nbc_len = read_u32(payload, 8)? as usize;
        if payload.len() < 12 + nbc_len + 4 {
            return None;
        }
        let nbc_bytes = payload[12..12 + nbc_len].to_vec();
        let json_off = 12 + nbc_len;
        let json_len = read_u32(payload, json_off)? as usize;
        if payload.len() < json_off + 4 + json_len + 8 {
            return None;
        }
        let snapshot_json = payload[json_off + 4..json_off + 4 + json_len].to_vec();
        let epoch = read_u64(payload, json_off + 4 + json_len)?;
        Some(Packet::ShadowReplicate {
            actor_id,
            nbc_bytes,
            snapshot_json,
            epoch,
        })
    }

    fn read_fetch_behavior_request(payload: &[u8]) -> Option<Self> {
        if payload.len() < 32 {
            return None;
        }
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(&payload[..32]);
        Some(Packet::FetchBehaviorRequest { content_hash })
    }

    fn read_fetch_behavior_response(payload: &[u8]) -> Option<Self> {
        if payload.len() < 34 {
            return None;
        }
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(&payload[..32]);
        let (behavior_name, name_len) = read_string(payload, 32)?;
        let off = 32 + name_len;
        if payload.len() < off + 4 {
            return None;
        }
        let byte_len = u32::from_be_bytes([
            payload[off],
            payload[off + 1],
            payload[off + 2],
            payload[off + 3],
        ]) as usize;
        let nbc_bytes = if byte_len == 0 {
            None
        } else {
            let start = off + 4;
            if payload.len() < start + byte_len {
                return None;
            }
            Some(payload[start..start + byte_len].to_vec())
        };
        Some(Packet::FetchBehaviorResponse {
            content_hash,
            behavior_name,
            nbc_bytes,
        })
    }
}
// ---------------------------------------------------------------------------
// Value (de)serialization helpers
// ---------------------------------------------------------------------------

// Type tags for Value variants.
const VAL_INT: u8 = 0;
const VAL_FLOAT: u8 = 1;
const VAL_BOOL: u8 = 2;
const VAL_STRING: u8 = 3;
const VAL_UNIT: u8 = 4;
const VAL_NIL: u8 = 5;
/// Varint-encoded i64 (future wire format). Tag byte followed by
/// 1–9 bytes of unsigned LEB128 encoding a zigzag-mapped i64.
/// Not yet used on the wire — reserved for version-bumped connections.
const VAL_INT_VARINT: u8 = 6;
const VAL_OBJECT: u8 = 7;

// ---------------------------------------------------------------------------
// Varint (unsigned LEB128) encoding — for future compact wire format
// ---------------------------------------------------------------------------

/// Write varint — used by tests via VAL_INT_VARINT roundtrip.
#[allow(dead_code)]
fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn read_varint(bytes: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut consumed = 0;
    loop {
        let byte = *bytes.get(offset + consumed)?;
        consumed += 1;
        result |= ((byte & 0x7F) as u64) << ((consumed - 1) * 7);
        if byte & 0x80 == 0 {
            return Some((result, consumed));
        }
        if consumed >= 10 {
            return None; // overflow
        }
    }
}

/// Zigzag encode — used by tests via VAL_INT_VARINT roundtrip.
#[allow(dead_code)]
fn zigzag_encode(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
fn zigzag_decode(v: u64) -> i64 {
    ((v >> 1) as i64) ^ (-((v & 1) as i64))
}
/// Write a [`Value`] into `buf`.
fn write_value(buf: &mut Vec<u8>, v: &Value) {
    if let Some(i) = v.as_int() {
        buf.push(VAL_INT);
        buf.extend_from_slice(&i.to_be_bytes());
    } else if let Some(f) = v.as_float() {
        buf.push(VAL_FLOAT);
        buf.extend_from_slice(&f.to_be_bytes());
    } else if let Some(b) = v.as_bool() {
        buf.push(VAL_BOOL);
        buf.push(if b { 1 } else { 0 });
    } else if let Some(id) = v.as_string_id() {
        // The id indexes the enclosing packet's string table, not any
        // constant pool — see `Packet::ActorMessage::string_table`.
        buf.push(VAL_STRING);
        buf.extend_from_slice(&id.to_be_bytes());
    } else if let Some(id) = v.as_object_id() {
        // The id indexes the enclosing packet's object table, not any local
        // object store — see `Packet::ActorMessage::object_table`.
        buf.push(VAL_OBJECT);
        buf.extend_from_slice(&id.to_be_bytes());
    } else if v.is_unit() {
        buf.push(VAL_UNIT);
    } else if v.is_nil() {
        buf.push(VAL_NIL);
    } else {
        // Fall back to writing raw bits as float (for NaN floats or other tagged NaNs)
        buf.push(VAL_FLOAT);
        buf.extend_from_slice(&v.as_raw().to_be_bytes());
    }
}

/// A [`Value`] is wire-safe only if it can cross to another node without
/// silent corruption: int, float, bool, nil, or unit always qualify. A heap
/// pointer is process-local, so those are always rejected. A string-id is
/// safe only when `strings_ok` — i.e. the enclosing packet carries a string
/// table with the content (actor messages do; spawn requests do not).
#[cfg(feature = "tcp")]
fn value_is_wire_safe(v: &Value, strings_ok: bool) -> bool {
    !(v.is_ptr() || v.is_actor_ref() || v.is_closure())
        && (strings_ok || !v.is_string())
        && (strings_ok || !v.is_object())
}

/// True if every payload [`Value`] carried by `packet` is wire-safe.
///
/// Only actor messages and spawn requests carry `Value`s; all other packet
/// kinds serialize plain scalars and are always safe to send. Actor-message
/// strings must additionally index the packet's string table — a string id
/// without a table entry is a dangling reference and is rejected. Spawn
/// requests keep strings rejected entirely: remotely-spawned actors run
/// native handlers and have no module pool to intern content into.
#[cfg(feature = "tcp")]
fn packet_payload_wire_safe(packet: &Packet) -> bool {
    match packet {
        Packet::ActorMessage {
            payload,
            string_table,
            object_table,
            ..
        } => payload.iter().all(|v| {
            value_is_wire_safe(v, true)
                && v.as_string_id()
                    .map_or(true, |id| (id as usize) < string_table.len())
                && v.as_object_id()
                    .map_or(true, |id| (id as usize) < object_table.len())
        }),
        Packet::SpawnRequest { initial_state, .. } => initial_state
            .iter()
            .all(|(_, v)| value_is_wire_safe(v, false)),
        _ => true,
    }
}

/// Read a [`Value`] from `bytes` starting at `offset`.
///
/// Returns `(Value, bytes_consumed)`.
fn read_value(bytes: &[u8], offset: usize) -> Option<(Value, usize)> {
    let tag = *bytes.get(offset)?;
    match tag {
        VAL_INT => {
            let v = read_i64(bytes, offset + 1)?;
            Some((Value::int(v), 1 + 8))
        }
        VAL_FLOAT => {
            let bits = read_u64(bytes, offset + 1)?;
            Some((Value::float(f64::from_bits(bits)), 1 + 8))
        }
        VAL_BOOL => {
            let b = *bytes.get(offset + 1)? != 0;
            Some((Value::bool(b), 1 + 1))
        }
        VAL_STRING => {
            let id = read_u32(bytes, offset + 1)?;
            Some((Value::string(id), 1 + 4))
        }
        VAL_OBJECT => {
            let id = read_u64(bytes, offset + 1)?;
            Some((Value::object(id), 1 + 8))
        }
        VAL_UNIT => Some((Value::unit(), 1)),
        VAL_NIL => Some((Value::nil(), 1)),
        VAL_INT_VARINT => {
            let (encoded, varint_len) = read_varint(bytes, offset + 1)?;
            Some((Value::int(zigzag_decode(encoded)), 1 + varint_len))
        }
        _ => None,
    }
}

/// Append a length-prefixed UTF-8 string.
fn write_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// Read a length-prefixed UTF-8 string.
///
/// Returns `(String, total_bytes_consumed)`.
fn read_string(bytes: &[u8], offset: usize) -> Option<(String, usize)> {
    let len = read_u32(bytes, offset)? as usize;
    let start = offset + 4;
    let end = start + len;
    if end > bytes.len() {
        return None;
    }
    let s = String::from_utf8(bytes[start..end].to_vec()).ok()?;
    Some((s, 4 + len))
}

/// Write an optional BLAKE3 content hash: 1-byte flag (0=None, 1=Some)
/// followed by 32 bytes when present.
fn write_optional_hash(buf: &mut Vec<u8>, hash: &Option<[u8; 32]>) {
    match hash {
        Some(h) => {
            buf.push(1);
            buf.extend_from_slice(h);
        }
        None => {
            buf.push(0);
        }
    }
}

/// Read an optional BLAKE3 content hash.
///
/// Returns `(Option<[u8; 32]>, bytes_consumed)`.
fn read_optional_hash(bytes: &[u8], offset: usize) -> Option<(Option<[u8; 32]>, usize)> {
    let flag = *bytes.get(offset)?;
    match flag {
        0 => Some((None, 1)),
        1 => {
            let start = offset + 1;
            let end = start + 32;
            if end > bytes.len() {
                return None;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&bytes[start..end]);
            Some((Some(hash), 33))
        }
        _ => None,
    }
}

/// Write optional byte blob: 1-byte flag (0=None, 1=Some) followed by
/// 4-byte length (u32, big-endian) and the bytes when present.

/// Read optional byte blob. Returns `(Option<Vec<u8>>, bytes_consumed)`.

// ---------------------------------------------------------------------------
// SocketAddr / NodeStatus (de)serialization helpers
// ---------------------------------------------------------------------------

/// Address family tags for [`write_addr`] / [`read_addr`].
const ADDR_IPV4: u8 = 4;
const ADDR_IPV6: u8 = 6;

/// Append a [`SocketAddr`] as `[family:u8][octets][port:u16]`.
fn write_addr(buf: &mut Vec<u8>, addr: &SocketAddr) {
    match addr {
        SocketAddr::V4(v4) => {
            buf.push(ADDR_IPV4);
            buf.extend_from_slice(&v4.ip().octets());
        }
        SocketAddr::V6(v6) => {
            buf.push(ADDR_IPV6);
            buf.extend_from_slice(&v6.ip().octets());
        }
    }
    buf.extend_from_slice(&addr.port().to_be_bytes());
}

/// Read a [`SocketAddr`] written by [`write_addr`].
///
/// Returns `(addr, bytes_consumed)`.
fn read_addr(bytes: &[u8], offset: usize) -> Option<(SocketAddr, usize)> {
    let family = *bytes.get(offset)?;
    let addr = match family {
        ADDR_IPV4 => {
            let octets: [u8; 4] = bytes.get(offset + 1..offset + 5)?.try_into().ok()?;
            let port = u16::from_be_bytes(bytes.get(offset + 5..offset + 7)?.try_into().ok()?);
            (
                SocketAddr::new(std::net::IpAddr::V4(octets.into()), port),
                1 + 4 + 2,
            )
        }
        ADDR_IPV6 => {
            let octets: [u8; 16] = bytes.get(offset + 1..offset + 17)?.try_into().ok()?;
            let port = u16::from_be_bytes(bytes.get(offset + 17..offset + 19)?.try_into().ok()?);
            (
                SocketAddr::new(std::net::IpAddr::V6(octets.into()), port),
                1 + 16 + 2,
            )
        }
        _ => return None,
    };
    Some(addr)
}

/// Map a [`NodeStatus`] to its wire byte.
fn status_to_u8(status: NodeStatus) -> u8 {
    match status {
        NodeStatus::Joining => 0,
        NodeStatus::Healthy => 1,
        NodeStatus::Suspicious => 2,
        NodeStatus::Failed => 3,
        NodeStatus::Leaving => 4,
    }
}

/// Inverse of [`status_to_u8`].
fn status_from_u8(b: u8) -> Option<NodeStatus> {
    match b {
        0 => Some(NodeStatus::Joining),
        1 => Some(NodeStatus::Healthy),
        2 => Some(NodeStatus::Suspicious),
        3 => Some(NodeStatus::Failed),
        4 => Some(NodeStatus::Leaving),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Little endian-agnostic integer readers / writers
// ---------------------------------------------------------------------------

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset + 8)?;
    let arr: [u8; 8] = slice.try_into().ok()?;
    Some(u64::from_be_bytes(arr))
}

fn read_i64(bytes: &[u8], offset: usize) -> Option<i64> {
    let slice = bytes.get(offset..offset + 8)?;
    let arr: [u8; 8] = slice.try_into().ok()?;
    Some(i64::from_be_bytes(arr))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    let arr: [u8; 4] = slice.try_into().ok()?;
    Some(u32::from_be_bytes(arr))
}

/// Acquire a mutex, recovering the guard even if a previous holder panicked.
///
/// Networking threads must keep running when one thread panics while holding
/// a shared lock; the data protected by these mutexes stays structurally
/// valid across panics, so poisoning is ignored rather than cascaded.
#[cfg(feature = "tcp")]
fn lock_ignore_poison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// TcpConnection
// ---------------------------------------------------------------------------

/// A single TCP connection to a remote node.
#[cfg(feature = "tcp")]
pub(crate) struct TcpConnection {
    #[allow(dead_code)]
    pub(crate) node_id: NodeId,
    pub(crate) addr: SocketAddr,
    pub(crate) stream: TransportStream,
    pub(crate) last_activity: Instant,
}

#[cfg(feature = "tcp")]
impl TcpConnection {
    /// Write a framed packet (length-prefixed) to the stream.
    fn send_packet(&mut self, packet_bytes: &[u8]) -> io::Result<()> {
        let len = packet_bytes.len() as u32;
        self.stream.write_all(&len.to_be_bytes())?;
        self.stream.write_all(packet_bytes)?;
        self.stream.flush()?;
        self.last_activity = Instant::now();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// IncomingPacket / OutgoingPacket
// ---------------------------------------------------------------------------

/// A packet that arrived from another node.
#[derive(Debug, Clone)]
pub struct IncomingPacket {
    pub from_node: NodeId,
    pub seq: u64,
    pub packet: Packet,
}

/// A packet to be sent to another node.
#[derive(Debug, Clone)]
pub struct OutgoingPacket {
    pub to_node: NodeId,
    pub to_addr: SocketAddr,
    pub packet: Packet,
}

// ---------------------------------------------------------------------------
// NetworkTransport
// ---------------------------------------------------------------------------

/// Manages all network connections for a Nulang node.
///
/// When created via [`bind`][TcpTransport::bind] the transport spawns
/// two long-lived background threads:
/// * a **listener** thread that accepts incoming TCP connections and
///   spawns a per-connection reader thread;
/// * a **sender** thread that dequeues [`OutgoingPacket`]s and writes
///   them to the appropriate TCP stream (connecting first if necessary).
pub trait NetworkTransport: Send {
    fn connect(&mut self, node_id: NodeId, addr: std::net::SocketAddr) -> std::io::Result<()>;
    fn send(&mut self, to_node: NodeId, to_addr: std::net::SocketAddr, packet: Packet);
    fn receive(&self) -> Vec<IncomingPacket>;
    fn node_id(&self) -> NodeId;
    fn listen_addr(&self) -> std::net::SocketAddr;
    fn disconnect(&mut self, node_id: NodeId);
    fn shutdown(&mut self);
    fn connection_count(&self) -> usize;
    fn connection_addr(&self, node_id: NodeId) -> Option<std::net::SocketAddr>;
    /// Simulate a network partition: silently drop every outbound packet
    /// to the given peers (as if a firewall dropped them in flight).
    /// Clearing the set (empty) restores normal delivery. Transports that
    /// cannot inject partitions leave this a no-op.
    fn set_partition(&mut self, peers: HashSet<NodeId>) {
        let _ = peers;
    }
    /// Enable bounded adjacent packet reordering on every link (DST
    /// fault injection): consecutive packets to a peer can arrive
    /// swapped — nothing is lost or duplicated, only delayed one slot.
    /// Deterministic by construction (per-pair counter, no RNG).
    /// Transports that cannot reorder leave this a no-op.
    fn set_reorder(&mut self, _enabled: bool) {}
    /// Deliver any packets still held by the bounded-reorder buffer
    /// (called by the harness at the end of a node's turn so odd tails
    /// are never stranded). Default: no-op.
    fn flush_held(&mut self) {}
}

impl NetworkTransport for Box<dyn NetworkTransport> {
    fn connect(&mut self, node_id: NodeId, addr: std::net::SocketAddr) -> std::io::Result<()> {
        (**self).connect(node_id, addr)
    }
    fn send(&mut self, to_node: NodeId, to_addr: std::net::SocketAddr, packet: Packet) {
        (**self).send(to_node, to_addr, packet)
    }
    fn receive(&self) -> Vec<IncomingPacket> {
        (**self).receive()
    }
    fn node_id(&self) -> NodeId {
        (**self).node_id()
    }
    fn listen_addr(&self) -> std::net::SocketAddr {
        (**self).listen_addr()
    }
    fn disconnect(&mut self, node_id: NodeId) {
        (**self).disconnect(node_id)
    }
    fn shutdown(&mut self) {
        (**self).shutdown()
    }
    fn connection_count(&self) -> usize {
        (**self).connection_count()
    }
    fn connection_addr(&self, node_id: NodeId) -> Option<std::net::SocketAddr> {
        (**self).connection_addr(node_id)
    }
    fn set_partition(&mut self, peers: HashSet<NodeId>) {
        (**self).set_partition(peers)
    }
    fn set_reorder(&mut self, enabled: bool) {
        (**self).set_reorder(enabled)
    }
    fn flush_held(&mut self) {
        (**self).flush_held()
    }
}
#[cfg(feature = "tcp")]
pub struct TcpTransport {
    node_id: NodeId,
    listen_addr: SocketAddr,
    /// Active connections to other nodes.
    connections: Arc<Mutex<HashMap<NodeId, TcpConnection>>>,
    /// Channel endpoint used to receive packets from other nodes.
    incoming_rx: mpsc::Receiver<IncomingPacket>,
    /// Cloneable send side of the incoming channel, handed to reader
    /// threads spawned for dialled outbound connections.
    incoming_tx: mpsc::SyncSender<IncomingPacket>,
    /// Channel endpoint used to enqueue packets for transmission.
    outgoing_tx: mpsc::SyncSender<OutgoingPacket>,
    /// Background thread handles.
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Flag used to ask background threads to shut down.
    shutdown_flag: Arc<AtomicBool>,
    /// TLS configuration. `PlaintextInsecure` means no encryption.
    tls_config: TlsConfig,
    /// Peers whose outbound packets are silently dropped (simulated
    /// network partition, see [`NetworkTransport::set_partition`]).
    partition: HashSet<NodeId>,
}

#[cfg(feature = "tcp")]
impl TcpTransport {
    /// Create and bind a new network transport.
    ///
    /// The listener is bound to `addr`.  If `addr` has port `0` an
    /// ephemeral port is chosen by the OS and can be queried later via
    /// [`listen_addr`][NetworkTransport::listen_addr].
    ///
    /// When `tls_config` is `MutualTls`, the node's identity is derived
    /// from the server certificate fingerprint (BLAKE3), not the socket
    /// address — this prevents identity spoofing. When `PlaintextInsecure`,
    /// identity falls back to the address hash (backward compatible).
    ///
    /// Two background threads are started:
    /// 1. **Listener** – accepts incoming TCP connections.
    /// 2. **Sender** – drains the outgoing queue and writes to TCP streams.
    pub fn bind(addr: SocketAddr, tls_config: TlsConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let listen_addr = listener.local_addr()?;
        let node_id = if let Some(cert_der) = tls_config.server_cert_der() {
            NodeId::from_cert_der(&cert_der)
        } else {
            NodeId::new(&listen_addr)
        };

        // Bounded channels.
        let (incoming_tx, incoming_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (outgoing_tx, outgoing_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);

        let connections: Arc<Mutex<HashMap<NodeId, TcpConnection>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(4);

        // ------------------------------------------------------------------
        // Listener thread
        // ------------------------------------------------------------------
        {
            let flag = Arc::clone(&shutdown_flag);
            let in_tx = incoming_tx.clone();
            let conns = Arc::clone(&connections);
            let local_id = node_id;
            let tls = tls_config.clone();
            let handle = thread::Builder::new()
                .name("nulang-net-listener".into())
                .spawn(move || {
                    listener_thread(listener, in_tx, conns, flag, local_id, tls);
                })?;
            handles.push(handle);
        }

        // ------------------------------------------------------------------
        // Sender thread
        // ------------------------------------------------------------------
        {
            let flag = Arc::clone(&shutdown_flag);
            let conns = Arc::clone(&connections);
            let local_id = node_id;
            let in_tx = incoming_tx.clone();
            let tls = tls_config.clone();
            let handle = thread::Builder::new()
                .name("nulang-net-sender".into())
                .spawn(move || {
                    sender_thread(outgoing_rx, conns, flag, local_id, in_tx, tls);
                })?;
            handles.push(handle);
        }

        Ok(TcpTransport {
            node_id,
            listen_addr,
            connections,
            incoming_rx,
            incoming_tx,
            outgoing_tx,
            threads: Arc::new(Mutex::new(handles)),
            shutdown_flag,
            tls_config,
            partition: HashSet::new(),
        })
    }

    /// Connect to a remote node.
    ///
    /// Establishes a TCP connection, performs the NUL0 versioned handshake,
    /// and registers the connection in the connection pool.
    ///
    /// When TLS is active, the peer's certificate fingerprint is verified
    /// against the expected `node_id` — a mismatch means the peer is
    /// presenting a different certificate than expected (spoofing or
    /// misconfiguration) and the connection is refused.
    pub fn connect(&mut self, node_id: NodeId, addr: SocketAddr) -> io::Result<()> {
        {
            let conns = lock_ignore_poison(&self.connections);
            if conns.contains_key(&node_id) {
                return Ok(());
            }
        }

        let tcp = TcpStream::connect_timeout(&addr, IO_TIMEOUT)?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))?;
        tcp.set_write_timeout(Some(IO_TIMEOUT))?;
        tcp.set_nodelay(true)?;

        let mut stream = if self.tls_config.is_plaintext() {
            TransportStream::Raw(tcp)
        } else {
            tls_wrap_client(tcp, &self.tls_config)?
        };

        // When TLS is active, verify the peer's certificate matches the
        // expected node identity *before* accepting the NUL0 handshake.
        if !self.tls_config.is_plaintext() {
            if let Some(cert_id) = stream.peer_cert_node_id() {
                if cert_id != node_id {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "TLS cert identity mismatch: expected {:?}, cert fingerprint {:?}",
                            node_id, cert_id
                        ),
                    ));
                }
            }
        }

        write_handshake(&mut stream, self.node_id)?;
        let peer_id = read_handshake(&mut stream)?;

        if peer_id != node_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "handshake mismatch: expected {:?}, got {:?}",
                    node_id, peer_id
                ),
            ));
        }

        let conn = TcpConnection {
            node_id,
            addr,
            stream,
            last_activity: Instant::now(),
        };

        // Short read timeout so the reader periodically releases the Mutex,
        // giving the sender thread windows to interleave writes.
        let _ = conn
            .stream
            .set_read_timeout(Some(Duration::from_millis(50)));

        let read_stream = conn.stream.try_clone()?;
        {
            let mut conns = lock_ignore_poison(&self.connections);
            conns.insert(node_id, conn);
        }
        let in_tx = self.incoming_tx.clone();
        let conns = Arc::clone(&self.connections);
        let flag = Arc::clone(&self.shutdown_flag);
        let _ = thread::Builder::new()
            .name(format!("nulang-net-reader-out-{}", addr.port()))
            .spawn(move || connection_read_loop(read_stream, node_id, in_tx, conns, flag));
        Ok(())
    }

    /// Send a packet to a remote node.
    ///
    /// A monotonically-increasing sequence number is attached automatically.
    /// The packet is enqueued on the outgoing channel; the background sender
    /// thread will establish a connection (if necessary) and write the
    /// packet to the wire.
    ///
    /// **Backpressure:** the outgoing channel is bounded
    /// ([`CHANNEL_CAPACITY`] packets). When it is full this call *blocks*
    /// until the sender thread drains a slot — this is deliberate
    /// backpressure toward the caller (typically the scheduler thread), not
    /// a silent drop. A packet is dropped only if the sender thread has
    /// already shut down (channel disconnected); that case is logged.
    pub fn send(&mut self, to_node: NodeId, to_addr: SocketAddr, packet: Packet) {
        // Reject payloads that cannot cross the wire losslessly. A heap
        // pointer is process-local and nil has no exact wire form; a string
        // id is only meaningful paired with the packet's string table.
        // Drop the packet loudly instead of silently mangling it.
        if !packet_payload_wire_safe(&packet) {
            warn!(
                "nulang-net: dropping packet to node {:?} (addr {}): payload value cannot cross the wire (heap pointer, nil, or string without content)",
                to_node, to_addr
            );
            return;
        }
        // Simulated partition: silently drop the packet exactly like a
        // firewall between the two nodes would. The peer sees the link
        // go quiet and the failure detector handles it from there.
        if self.partition.contains(&to_node) {
            return;
        }
        let outgoing = OutgoingPacket {
            to_node,
            to_addr,
            packet,
        };
        // Blocks on a full channel (backpressure). An error means the sender
        // thread has shut down and the packet cannot be delivered — log it
        // rather than dropping silently.
        if self.outgoing_tx.send(outgoing).is_err() {
            warn!(
                "nulang-net: dropping packet to node {:?} (addr {}): sender thread shut down",
                to_node, to_addr
            );
        }
    }

    /// Receive incoming packets (non-blocking).
    ///
    /// Returns all packets that have arrived since the last call.
    pub fn receive(&self) -> Vec<IncomingPacket> {
        let mut packets = Vec::new();
        loop {
            match self.incoming_rx.try_recv() {
                Ok(p) => packets.push(p),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        packets
    }

    /// Get this node's ID.
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// Get the listen address.
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Disconnect from a remote node.
    ///
    /// Closes the TCP stream and removes the entry from the connection pool.
    pub fn disconnect(&mut self, node_id: NodeId) {
        let mut conns = lock_ignore_poison(&self.connections);
        if let Some(conn) = conns.remove(&node_id) {
            let _ = conn.stream.shutdown();
        }
    }

    /// Shutdown the transport cleanly.
    ///
    /// Signals all background threads to stop, joins them, and closes
    /// every active TCP connection.
    pub fn shutdown(&mut self) {
        // Signal shutdown.
        self.shutdown_flag.store(true, Ordering::SeqCst);

        // Drop the outgoing sender so the sender thread wakes up and exits.
        let _ = std::mem::replace(&mut self.outgoing_tx, mpsc::sync_channel(1).0);

        // Close all connections so reader threads unblock.
        {
            let conns = lock_ignore_poison(&self.connections);
            for (_, conn) in conns.iter() {
                let _ = conn.stream.shutdown();
            }
        }

        // Join all background threads.
        let handles: Vec<_> = {
            let mut guard = lock_ignore_poison(&self.threads);
            guard.drain(..).collect()
        };
        for h in handles {
            let _ = h.join();
        }
    }

    /// Get the number of active connections.
    pub fn connection_count(&self) -> usize {
        let conns = lock_ignore_poison(&self.connections);
        conns.len()
    }

    /// Look up the remote address of an active connection by node id.
    ///
    /// For connections we dialled this is the peer's listen address; for
    /// accepted inbound connections it is the peer's (ephemeral) source
    /// address. Either way it identifies a live link to the peer, which
    /// is enough for heartbeat-based membership discovery while the
    /// connection is open.
    pub fn connection_addr(&self, node_id: NodeId) -> Option<SocketAddr> {
        let conns = lock_ignore_poison(&self.connections);
        conns.get(&node_id).map(|conn| conn.addr)
    }
}

// ---------------------------------------------------------------------------
// Background thread implementations
// ---------------------------------------------------------------------------

/// Listener thread entry point.
///
/// Accepts incoming TCP connections.  For each accepted stream a new
/// "reader" thread is spawned that performs the handshake and then
/// enters a read-loop deserialising packets.
#[cfg(feature = "tcp")]
fn listener_thread(
    listener: TcpListener,
    incoming_tx: mpsc::SyncSender<IncomingPacket>,
    connections: Arc<Mutex<HashMap<NodeId, TcpConnection>>>,
    shutdown_flag: Arc<AtomicBool>,
    local_node_id: NodeId,
    tls_config: TlsConfig,
) {
    // Set a small accept timeout so we periodically check the shutdown flag.
    let _ = listener.set_nonblocking(true);

    loop {
        if shutdown_flag.load(Ordering::Relaxed) {
            break;
        }

        match listener.accept() {
            Ok((stream, addr)) => {
                let in_tx = incoming_tx.clone();
                let conns = Arc::clone(&connections);
                let flag = Arc::clone(&shutdown_flag);
                let tls = tls_config.clone();
                let _ = thread::Builder::new()
                    .name(format!("nulang-net-reader-{}", addr.port()))
                    .spawn(move || {
                        connection_reader(stream, addr, in_tx, conns, flag, local_node_id, tls);
                    });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                // Listener socket broken — time to exit.
                break;
            }
        }
    }
}

/// Per-connection reader thread.
///
/// 1. Sends our node-id (8 bytes).
/// 2. Reads the peer's node-id (8 bytes).
/// 3. Registers the connection.
/// 4. Reads framed packets in a loop until disconnect or shutdown.
#[cfg(feature = "tcp")]
fn connection_reader(
    tcp: TcpStream,
    addr: SocketAddr,
    incoming_tx: mpsc::SyncSender<IncomingPacket>,
    connections: Arc<Mutex<HashMap<NodeId, TcpConnection>>>,
    shutdown_flag: Arc<AtomicBool>,
    local_node_id: NodeId,
    tls_config: TlsConfig,
) {
    let _ = tcp.set_read_timeout(Some(IO_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(IO_TIMEOUT));
    let _ = tcp.set_nodelay(true);

    let mut stream = if tls_config.is_plaintext() {
        TransportStream::Raw(tcp)
    } else {
        match tls_wrap_server(tcp, &tls_config) {
            Ok(s) => s,
            Err(e) => {
                warn!("TLS accept failed for {}: {}", addr, e);
                return;
            }
        }
    };

    if write_handshake(&mut stream, local_node_id).is_err() {
        return;
    }

    let peer_id = match read_handshake(&mut stream) {
        Ok(id) => id,
        Err(_) => return,
    };

    // When TLS is active, verify the peer's certificate fingerprint
    // matches the node_id claimed in the NUL0 handshake.
    if !tls_config.is_plaintext() {
        if let Some(cert_id) = stream.peer_cert_node_id() {
            if cert_id != peer_id {
                warn!(
                    "TLS cert identity mismatch for {}: handshake claims {:?}, cert fingerprint {:?}",
                    addr, peer_id, cert_id
                );
                return;
            }
        }
    }

    // Short read timeout so the reader periodically releases the Mutex,
    // giving the sender thread windows to interleave writes.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));

    {
        let mut conns = lock_ignore_poison(&connections);
        conns.insert(
            peer_id,
            TcpConnection {
                node_id: peer_id,
                addr,
                stream: stream.try_clone().expect("try_clone should succeed"),
                last_activity: Instant::now(),
            },
        );
    }

    connection_read_loop(stream, peer_id, incoming_tx, connections, shutdown_flag);
}

/// Read framed packets from `stream` until disconnect or shutdown, then
/// remove the peer from the connection pool.
///
/// Shared by the listener-side reader (accepted inbound connections) and
/// the reader spawned for dialled outbound connections, so every TCP
/// link is read exactly once regardless of which side initiated it.
#[cfg(feature = "tcp")]
fn connection_read_loop(
    mut stream: TransportStream,
    peer_id: NodeId,
    incoming_tx: mpsc::SyncSender<IncomingPacket>,
    connections: Arc<Mutex<HashMap<NodeId, TcpConnection>>>,
    shutdown_flag: Arc<AtomicBool>,
) {
    loop {
        if shutdown_flag.load(Ordering::Relaxed) {
            break;
        }

        // Read 4-byte length prefix.
        let len = loop {
            let mut len_buf = [0u8; 4];
            match stream.read_exact(&mut len_buf) {
                Ok(()) => break u32::from_be_bytes(len_buf),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // TLS read timeout: release lock briefly so the
                    // sender thread can acquire it.
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(_) => return, // Disconnect or timeout.
            }
        };
        if len == 0 || len > MAX_PACKET_LEN {
            return; // Protocol error or DoS.
        }

        // Read payload.
        let mut payload = vec![0u8; len as usize];
        loop {
            match stream.read_exact(&mut payload) {
                Ok(()) => break,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(_) => return,
            }
        }

        // Update last-activity timestamp.
        {
            let mut conns = lock_ignore_poison(&connections);
            if let Some(conn) = conns.get_mut(&peer_id) {
                conn.last_activity = Instant::now();
            }
        }

        if let Some((seq, packet)) = Packet::from_bytes(&payload) {
            let incoming = IncomingPacket {
                from_node: peer_id,
                seq,
                packet,
            };
            if incoming_tx.send(incoming).is_err() {
                break;
            }
        }
    }
    {
        let mut conns = lock_ignore_poison(&connections);
        conns.remove(&peer_id);
    }
    let _ = stream.shutdown();
}

/// Sender thread entry point.
///
/// Drains the outgoing queue, looks up (or creates) the TCP connection
/// for each packet, and writes the framed bytes.
#[cfg(feature = "tcp")]
fn sender_thread(
    outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    connections: Arc<Mutex<HashMap<NodeId, TcpConnection>>>,
    shutdown_flag: Arc<AtomicBool>,
    local_node_id: NodeId,
    incoming_tx: mpsc::SyncSender<IncomingPacket>,
    tls_config: TlsConfig,
) {
    // We keep a local sequence counter so we can embed it into the bytes.
    let mut next_seq: u64 = 1;

    loop {
        if shutdown_flag.load(Ordering::Relaxed) && outgoing_rx.try_recv().is_err() {
            break;
        }

        let outgoing = match outgoing_rx.recv_timeout(CHANNEL_RECV_TIMEOUT) {
            Ok(p) => p,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Look up connection.
        let mut needs_connect = false;
        {
            let conns = lock_ignore_poison(&connections);
            if !conns.contains_key(&outgoing.to_node) {
                needs_connect = true;
            }
        }

        // Establish connection if missing.
        if needs_connect {
            if let Err(e) = connect_in_sender(
                &connections,
                &incoming_tx,
                &shutdown_flag,
                local_node_id,
                outgoing.to_node,
                outgoing.to_addr,
                &tls_config,
            ) {
                warn!(
                    "[nulang-net] Failed to connect to {:?} at {}: {}",
                    outgoing.to_node, outgoing.to_addr, e
                );
            }
        }

        // Send the packet.
        let seq = next_seq;
        next_seq = next_seq.wrapping_add(1);
        let bytes = outgoing.packet.to_bytes(seq);

        let result = {
            let mut conns = lock_ignore_poison(&connections);
            if let Some(conn) = conns.get_mut(&outgoing.to_node) {
                conn.send_packet(&bytes)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "connection disappeared",
                ))
            }
        };

        if let Err(e) = result {
            warn!(
                "[nulang-net] Send to {:?} failed: {}; removing connection",
                outgoing.to_node, e
            );
            let mut conns = lock_ignore_poison(&connections);
            if let Some(conn) = conns.remove(&outgoing.to_node) {
                let _ = conn.stream.shutdown();
            }
        }
    }
}

/// Establish a TCP connection from inside the sender thread.
///
/// This is a best-effort connect that performs the 8-byte handshake.
/// A reader thread is spawned on a cloned handle so the link is fully
/// duplex: without it, a node that only ever dials out (e.g. a cluster
/// joiner) could never receive packets over the connection it
/// established, and heartbeat replies from its seed would be lost.
#[cfg(feature = "tcp")]
fn connect_in_sender(
    connections: &Arc<Mutex<HashMap<NodeId, TcpConnection>>>,
    incoming_tx: &mpsc::SyncSender<IncomingPacket>,
    shutdown_flag: &Arc<AtomicBool>,
    local_node_id: NodeId,
    node_id: NodeId,
    addr: SocketAddr,
    tls_config: &TlsConfig,
) -> io::Result<()> {
    let tcp = TcpStream::connect_timeout(&addr, IO_TIMEOUT)?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
    tcp.set_nodelay(true)?;

    let mut stream = if tls_config.is_plaintext() {
        TransportStream::Raw(tcp)
    } else {
        tls_wrap_client(tcp, tls_config)?
    };

    // When TLS is active, verify the peer's certificate matches the
    // expected node identity.
    if !tls_config.is_plaintext() {
        if let Some(cert_id) = stream.peer_cert_node_id() {
            if cert_id != node_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "TLS cert identity mismatch in sender connect",
                ));
            }
        }
    }

    write_handshake(&mut stream, local_node_id)?;
    let peer_id = read_handshake(&mut stream)?;

    if peer_id != node_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "node id handshake mismatch in sender connect",
        ));
    }

    let read_stream = stream.try_clone()?;
    {
        let mut conns = lock_ignore_poison(connections);
        conns.insert(
            node_id,
            TcpConnection {
                node_id,
                addr,
                stream,
                last_activity: Instant::now(),
            },
        );
    }
    let in_tx = incoming_tx.clone();
    let conns = Arc::clone(connections);
    let flag = Arc::clone(shutdown_flag);
    let _ = thread::Builder::new()
        .name(format!("nulang-net-reader-out-{}", addr.port()))
        .spawn(move || connection_read_loop(read_stream, node_id, in_tx, conns, flag));
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::crdt_manager::{CrdtId, CrdtType};
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::thread::sleep;

    // ------------------------------------------------------------------
    // 1. NodeId hashing
    // ------------------------------------------------------------------
    #[test]
    fn test_node_id_from_addr() {
        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9000);
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9001);
        let addr3 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9000);

        let id1 = NodeId::new(&addr1);
        let id2 = NodeId::new(&addr2);
        let id3 = NodeId::new(&addr3);

        assert_eq!(id1, id3, "same address must produce same NodeId");
        assert_ne!(
            id1, id2,
            "different addresses must produce different NodeId"
        );
        assert_ne!(id1.0, 0, "NodeId must not be zero");
    }

    // ------------------------------------------------------------------
    // 2. ActorMessage roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_serialize_deserialize_actor_message() {
        let packet = Packet::ActorMessage {
            target_actor: 42,
            behavior_name: "handle_msg".to_string(),
            content_hash: None,
            payload: vec![Value::int(123), Value::string(456)],
            string_table: vec![],
            object_table: vec![],
            sender_actor: 99,
            sender_node: NodeId(0xDEAD_BEEF_CAFE_BABE),
            priority: MessagePriority::Normal,
            trace_id: None,
        };

        let bytes = packet.to_bytes(0x1234);
        let (seq, decoded) = Packet::from_bytes(&bytes).expect("deserialization failed");

        assert_eq!(seq, 0x1234);
        assert_eq!(decoded, packet);
    }

    // ------------------------------------------------------------------
    // 2b. ActorMessage string table roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_actor_message_string_table_roundtrip() {
        let packet = Packet::ActorMessage {
            target_actor: 7,
            behavior_name: "store".to_string(),
            content_hash: None,
            payload: vec![Value::string(0), Value::string(1), Value::string(0)],
            string_table: vec!["hello".to_string(), "wörld ✓".to_string()],
            object_table: vec![],
            sender_actor: 3,
            sender_node: NodeId(0x1111_2222_3333_4444),
            priority: MessagePriority::Normal,
            trace_id: None,
        };

        let bytes = packet.to_bytes(77);
        let (seq, decoded) =
            Packet::from_bytes(&bytes).expect("actor message deserialization failed");

        assert_eq!(seq, 77);
        assert_eq!(decoded, packet);
    }

    // ------------------------------------------------------------------
    // 2c. ActorMessage object table roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_actor_message_object_table_roundtrip() {
        let packet = Packet::ActorMessage {
            target_actor: 8,
            behavior_name: "handle_bytes".to_string(),
            content_hash: None,
            payload: vec![Value::object(0), Value::object(1), Value::object(0)],
            string_table: vec![],
            object_table: vec![(0, vec![1, 2, 3]), (1, vec![4, 5, 6, 7])],
            sender_actor: 4,
            sender_node: NodeId(0x2222_3333_4444_5555),
            priority: MessagePriority::Normal,
            trace_id: None,
        };

        let bytes = packet.to_bytes(88);
        let (seq, decoded) =
            Packet::from_bytes(&bytes).expect("actor message object table deserialization failed");

        assert_eq!(seq, 88);
        assert_eq!(decoded, packet);
    }

    #[test]
    fn test_packet_actor_message_rejects_truncated_string_table() {
        let packet = Packet::ActorMessage {
            target_actor: 7,
            behavior_name: "store".to_string(),
            content_hash: None,
            payload: vec![Value::string(0)],
            string_table: vec!["hello".to_string()],
            object_table: vec![],
            sender_actor: 3,
            sender_node: NodeId(1),
            priority: MessagePriority::Normal,
            trace_id: None,
        };
        let bytes = packet.to_bytes(1);
        // Chop the string table in half: the declared count/content no
        // longer fits, so deserialization must fail cleanly (no panic).
        let truncated = &bytes[..bytes.len() - 3];
        assert!(Packet::from_bytes(truncated).is_none());
        // A packet cut off right after the payload values (before the
        // table count: 4 bytes count + 4 bytes len + 5 bytes "hello") is
        // rejected too.
        let values_end = bytes.len() - 13;
        assert!(Packet::from_bytes(&bytes[..values_end]).is_none());
    }

    // ------------------------------------------------------------------
    // 3b. Gossip roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_serialize_deserialize_gossip() {
        let packet = Packet::Gossip {
            members: vec![
                NodeGossip {
                    node_id: NodeId(0x1111_2222_3333_4444),
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9100),
                    status: NodeStatus::Healthy,
                    incarnation: 7,
                },
                NodeGossip {
                    node_id: NodeId(0xAAAA_BBBB_CCCC_DDDD),
                    address: SocketAddr::new(IpAddr::V6("::1".parse().unwrap()), 49152),
                    status: NodeStatus::Suspicious,
                    incarnation: u64::MAX,
                },
                NodeGossip {
                    node_id: NodeId(1),
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 0),
                    status: NodeStatus::Joining,
                    incarnation: 0,
                },
            ],
            directory: vec![
                DurableDirectoryEntry {
                    actor_id: 7,
                    node_id: NodeId(0x1111_2222_3333_4444),
                    epoch: 1,
                },
                DurableDirectoryEntry {
                    actor_id: 9,
                    node_id: NodeId(0xAAAA_BBBB_CCCC_DDDD),
                    epoch: 3,
                },
            ],
        };

        let bytes = packet.to_bytes(99);
        let (seq, decoded) = Packet::from_bytes(&bytes).expect("gossip deserialization failed");

        assert_eq!(seq, 99);
        assert_eq!(decoded, packet);
    }

    #[test]
    fn test_packet_gossip_rejects_truncated_payload() {
        // A header followed by a truncated entry must not panic and must
        // fail cleanly.
        let packet = Packet::Gossip {
            members: vec![NodeGossip {
                node_id: NodeId(42),
                address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9100),
                status: NodeStatus::Healthy,
                incarnation: 3,
            }],
            directory: vec![],
        };
        let bytes = packet.to_bytes(1);
        // Keep the header + count, chop the entry in half.
        let truncated = &bytes[..bytes.len() - 5];
        assert!(Packet::from_bytes(truncated).is_none());
    }

    // ------------------------------------------------------------------
    // 3d. CRDT delta sync roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_serialize_deserialize_crdt_delta_sync() {
        let full_op = CrdtDeltaOp {
            op: CrdtOp {
                crdt_id: CrdtId(7),
                crdt_type: CrdtType::GCounter,
                payload: vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0],
            },
            is_delta: false,
        };
        let delta_op = CrdtDeltaOp {
            op: CrdtOp {
                crdt_id: CrdtId(7),
                crdt_type: CrdtType::GCounter,
                payload: vec![1, 2, 3],
            },
            is_delta: true,
        };
        let packet = Packet::CrdtDeltaSync {
            ops: Arc::new(vec![full_op, delta_op]),
        };

        let bytes = packet.to_bytes(42);
        let (seq, decoded) =
            Packet::from_bytes(&bytes).expect("crdt delta sync deserialization failed");

        assert_eq!(seq, 42);
        assert_eq!(decoded, packet);
    }

    #[test]
    fn test_packet_crdt_delta_sync_rejects_truncated_payload() {
        let packet = Packet::CrdtDeltaSync {
            ops: Arc::new(vec![CrdtDeltaOp {
                op: CrdtOp {
                    crdt_id: CrdtId(1),
                    crdt_type: CrdtType::GSet,
                    payload: vec![0xAB; 8],
                },
                is_delta: true,
            }]),
        };
        let bytes = packet.to_bytes(1);
        // Keep the header + count, chop the op in half.
        let truncated = &bytes[..bytes.len() - 3];
        assert!(Packet::from_bytes(truncated).is_none());
    }

    // ------------------------------------------------------------------
    // 3c. Spawn request/response roundtrips
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_serialize_deserialize_spawn_response() {
        let packet = Packet::SpawnResponse {
            request_id: 0xDEAD_BEEF,
            actor_id: 424242,
            success: true,
        };
        let bytes = packet.to_bytes(3);
        let (seq, decoded) = Packet::from_bytes(&bytes).unwrap();
        assert_eq!(seq, 3);
        assert_eq!(decoded, packet);
    }

    // ------------------------------------------------------------------
    // 3. Heartbeat roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_serialize_deserialize_heartbeat() {
        let packet = Packet::Heartbeat {
            node_id: NodeId(0xABCD),
            timestamp: 1_700_000_000_000,
        };

        let bytes = packet.to_bytes(7);
        let (seq, decoded) = Packet::from_bytes(&bytes).unwrap();

        assert_eq!(seq, 7);
        assert_eq!(decoded, packet);
    }

    // ------------------------------------------------------------------
    // 4. Int value roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_value_serialization_int() {
        let v = Value::int(-42_000_000_000_i64);
        let mut buf = Vec::new();
        write_value(&mut buf, &v);

        let (decoded, consumed) = read_value(&buf, 0).unwrap();
        assert_eq!(consumed, 9); // 1 tag + 8 bytes
        assert_eq!(decoded, v);
    }

    // ------------------------------------------------------------------
    // 5. String value roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_value_serialization_string() {
        let v = Value::string(42);
        let mut buf = Vec::new();
        write_value(&mut buf, &v);

        let (decoded, consumed) = read_value(&buf, 0).unwrap();
        assert_eq!(consumed, 5); // 1 tag + 4 bytes
        assert_eq!(decoded, v);
    }

    // ------------------------------------------------------------------
    // 6. Mixed payload roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_value_serialization_complex() {
        let values = vec![
            Value::int(0),
            Value::int(-1),
            Value::int(i64::MAX),
            Value::int(i64::MIN),
            Value::float(std::f64::consts::PI),
            Value::float(f64::NAN),
            Value::float(f64::INFINITY),
            Value::bool(true),
            Value::bool(false),
            Value::string(0),
            Value::string(1),
            Value::string(999),
            Value::unit(),
        ];

        for v in &values {
            let mut buf = Vec::new();
            write_value(&mut buf, v);
            let (decoded, _) = read_value(&buf, 0).unwrap();

            // For NaN, we need to compare bits because NaN != NaN.
            match (v.as_float(), decoded.as_float()) {
                (Some(a), Some(b)) if a.is_nan() && b.is_nan() => {}
                _ => assert_eq!(decoded, *v, "roundtrip failed for {:?}", v),
            }
        }
    }

    // ------------------------------------------------------------------
    // 6b. Varint encoding roundtrip (VAL_INT_VARINT)
    // ------------------------------------------------------------------
    #[test]
    fn test_varint_roundtrip_edge_cases() {
        let cases: &[i64] = &[
            0,
            1,
            -1,
            63,
            -64,
            127,
            -128,
            8191,
            -8192,
            i64::MAX,
            i64::MIN,
        ];
        for &val in cases {
            let v = Value::int(val);
            let mut buf = Vec::new();
            buf.push(VAL_INT_VARINT);
            write_varint(&mut buf, zigzag_encode(val));
            let (decoded, _) = read_value(&buf, 0).unwrap();
            assert_eq!(decoded, v, "varint roundtrip failed for {}", val);
        }
    }

    #[test]
    fn test_varint_small_int_is_compact() {
        // Values in [-64, 63] encode as 2 bytes (tag + 1 data byte).
        let v = Value::int(42);
        let mut buf = Vec::new();
        buf.push(VAL_INT_VARINT);
        write_varint(&mut buf, zigzag_encode(42));
        assert_eq!(buf.len(), 2);
        let (decoded, _) = read_value(&buf, 0).unwrap();
        assert_eq!(decoded, v);
    }

    // ------------------------------------------------------------------
    // 7. Transport bind
    // ------------------------------------------------------------------
    #[test]
    #[cfg(feature = "tcp")]
    fn test_transport_bind() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport =
            TcpTransport::bind(addr, crate::runtime::network::TlsConfig::PlaintextInsecure)
                .expect("bind failed");

        assert_eq!(
            transport.listen_addr().ip(),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_ne!(
            transport.listen_addr().port(),
            0,
            "ephemeral port must be assigned"
        );
        assert_eq!(transport.connection_count(), 0);
        assert_eq!(transport.node_id(), NodeId::new(&transport.listen_addr()));

        transport.shutdown();
    }

    // ------------------------------------------------------------------
    // 8. Two transports can connect
    // ------------------------------------------------------------------
    #[test]
    #[cfg(feature = "tcp")]
    fn test_transport_connect() {
        let addr_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_a = TcpTransport::bind(
            addr_a,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_b = TcpTransport::bind(
            addr_b,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b_actual = transport_b.listen_addr();
        let node_b_id = transport_b.node_id();

        // A connects to B.
        transport_a
            .connect(node_b_id, addr_b_actual)
            .expect("connect failed");

        // Give the listener thread a moment to accept and handshake.
        sleep(Duration::from_millis(100));

        // B should have an incoming connection from A.
        // (B does not explicitly connect back — the TCP connection is
        //  bidirectional, but B only learns about A when A sends a packet.
        //  The connection is stored in A's pool; B also stores it when
        //  the reader thread accepts it.)
        assert!(
            transport_a.connection_count() >= 1,
            "transport A should have at least one connection"
        );

        transport_a.shutdown();
        transport_b.shutdown();
    }

    // ------------------------------------------------------------------
    // 9. Send packet and receive on the other side
    // ------------------------------------------------------------------
    #[test]
    #[cfg(feature = "tcp")]
    fn test_transport_send_receive() {
        let addr_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_a = TcpTransport::bind(
            addr_a,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_b = TcpTransport::bind(
            addr_b,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b_actual = transport_b.listen_addr();
        let node_b_id = transport_b.node_id();

        // A connects to B.
        transport_a.connect(node_b_id, addr_b_actual).unwrap();
        sleep(Duration::from_millis(100));

        // A sends a packet to B.
        let packet = Packet::Heartbeat {
            node_id: transport_a.node_id(),
            timestamp: 1_700_000_000,
        };
        transport_a.send(node_b_id, addr_b_actual, packet.clone());

        // B should eventually receive it.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while Instant::now() < deadline && received.is_empty() {
            received = transport_b.receive();
            if received.is_empty() {
                sleep(Duration::from_millis(50));
            }
        }

        assert!(
            !received.is_empty(),
            "transport B should have received the heartbeat"
        );
        assert_eq!(received[0].from_node, transport_a.node_id());
        assert_eq!(received[0].packet, packet);

        transport_a.shutdown();
        transport_b.shutdown();
    }

    // ------------------------------------------------------------------
    // 9b. Non-scalar payloads are rejected at send time
    // ------------------------------------------------------------------
    #[test]
    #[cfg(feature = "tcp")]
    fn test_value_wire_safety_classification() {
        // Scalars round-trip exactly and are safe to send. Strings are safe
        // only where the packet can carry their content (`strings_ok`).
        assert!(value_is_wire_safe(&Value::int(1), false));
        assert!(value_is_wire_safe(&Value::float(2.5), false));
        assert!(value_is_wire_safe(&Value::bool(true), false));
        assert!(value_is_wire_safe(&Value::unit(), false));
        assert!(value_is_wire_safe(&Value::string(7), true));
        assert!(!value_is_wire_safe(&Value::string(7), false));

        // Heap/tagged values (except nil) would arrive corrupted on the
        // receiving node, so they must always be rejected. Nil is now
        // wire-safe (VAL_NIL tag).
        assert!(!value_is_wire_safe(&Value::ptr(std::ptr::null_mut()), true));
        assert!(!value_is_wire_safe(&Value::actor_ref(9), true));
        assert!(!value_is_wire_safe(&Value::closure(3), true));
        assert!(value_is_wire_safe(&Value::nil(), true));

        // Packet-level classification: an actor-message string id must
        // index the packet's string table.
        let mk = |payload: Vec<Value>, string_table: Vec<String>| Packet::ActorMessage {
            target_actor: 1,
            behavior_name: "h".into(),
            content_hash: None,
            payload,
            string_table,
            object_table: vec![],
            sender_actor: 0,
            sender_node: NodeId(5),
            priority: MessagePriority::Normal,
            trace_id: None,
        };
        assert!(packet_payload_wire_safe(&mk(vec![Value::int(1)], vec![])));
        assert!(packet_payload_wire_safe(&mk(
            vec![Value::string(0)],
            vec!["hello".into()]
        )));
        // Dangling id: no table entry at index 3.
        assert!(!packet_payload_wire_safe(&mk(
            vec![Value::string(3)],
            vec![]
        )));
        // Heap values stay rejected even with a table present.
        assert!(!packet_payload_wire_safe(&mk(
            vec![Value::ptr(std::ptr::null_mut())],
            vec!["x".into()]
        )));

        // Spawn requests have no receiving-side pool, so strings stay
        let spawn = Packet::SpawnRequest {
            request_id: 1,
            behavior_name: "Counter".into(),
            content_hash: None,
            initial_state: vec![("name".into(), Value::string(1))],
            bytecode: None,
        };
        assert!(!packet_payload_wire_safe(&spawn));

        assert!(packet_payload_wire_safe(&Packet::Heartbeat {
            node_id: NodeId(1),
            timestamp: 0,
        }));
    }

    #[test]
    fn test_nil_wire_roundtrip() {
        // Nil must serialize and deserialize as nil (not as a float).
        let mut buf = Vec::new();
        write_value(&mut buf, &Value::nil());
        assert!(!buf.is_empty());
        let (val, consumed) = read_value(&buf, 0).expect("nil should deserialize");
        assert!(val.is_nil(), "deserialized value must be nil");
        assert_eq!(consumed, 1, "nil tag has no payload bytes");

        // Roundtrip: nil written then read should match.
        let mut buf2 = Vec::new();
        write_value(&mut buf2, &val);
        assert_eq!(buf, buf2, "nil roundtrip must be stable");
    }

    #[test]
    #[cfg(feature = "tcp")]
    fn test_transport_send_rejects_dangling_string_payload() {
        let addr_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_a = TcpTransport::bind(
            addr_a,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();
        let addr_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_b = TcpTransport::bind(
            addr_b,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b_actual = transport_b.listen_addr();
        let node_b_id = transport_b.node_id();

        transport_a.connect(node_b_id, addr_b_actual).unwrap();
        sleep(Duration::from_millis(100));

        // A string id without a string-table entry is a dangling reference
        // that would resolve to the wrong string (or nil) on the receiving
        // node. The transport must drop the packet at send time rather than
        // deliver corrupt data.
        let bad = Packet::ActorMessage {
            target_actor: 1,
            behavior_name: "handle".into(),
            content_hash: None,
            payload: vec![Value::string(42)],
            string_table: vec![],
            object_table: vec![],
            sender_actor: 7,
            sender_node: transport_a.node_id(),
            priority: MessagePriority::Normal,
            trace_id: None,
        };
        transport_a.send(node_b_id, addr_b_actual, bad);

        // Give the (non-)delivery plenty of time, then confirm nothing came.
        sleep(Duration::from_millis(500));
        let received = transport_b.receive();
        assert!(
            received
                .iter()
                .all(|p| !matches!(p.packet, Packet::ActorMessage { .. })),
            "dangling string payload must be rejected at send time, but B received: {:?}",
            received
        );

        transport_a.shutdown();
        transport_b.shutdown();
    }

    #[test]
    #[cfg(feature = "tcp")]
    fn test_transport_send_delivers_string_payload_with_table() {
        let addr_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_a = TcpTransport::bind(
            addr_a,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();
        let addr_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_b = TcpTransport::bind(
            addr_b,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b_actual = transport_b.listen_addr();
        let node_b_id = transport_b.node_id();

        transport_a.connect(node_b_id, addr_b_actual).unwrap();
        sleep(Duration::from_millis(100));

        // A string payload whose content travels in the packet's string
        let good = Packet::ActorMessage {
            target_actor: 1,
            behavior_name: "handle".into(),
            content_hash: None,
            payload: vec![Value::string(0), Value::int(7), Value::string(1)],
            string_table: vec!["hello".into(), "world".into()],
            object_table: vec![],
            sender_actor: 7,
            sender_node: transport_a.node_id(),
            priority: MessagePriority::Normal,
            trace_id: None,
        };
        transport_a.send(node_b_id, addr_b_actual, good.clone());

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while Instant::now() < deadline && received.is_empty() {
            received = transport_b.receive();
            if received.is_empty() {
                sleep(Duration::from_millis(50));
            }
        }

        assert!(
            received.iter().any(|p| p.packet == good),
            "string payload with content table must be delivered, got: {:?}",
            received
        );

        transport_a.shutdown();
        transport_b.shutdown();
    }

    #[test]
    #[cfg(feature = "tcp")]
    fn test_transport_send_delivers_scalar_payload() {
        let addr_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_a = TcpTransport::bind(
            addr_a,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();
        let addr_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_b = TcpTransport::bind(
            addr_b,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b_actual = transport_b.listen_addr();
        let node_b_id = transport_b.node_id();

        transport_a.connect(node_b_id, addr_b_actual).unwrap();
        sleep(Duration::from_millis(100));

        // Scalar payloads are wire-safe and must be delivered unchanged —
        let good = Packet::ActorMessage {
            target_actor: 1,
            behavior_name: "handle".into(),
            content_hash: None,
            payload: vec![Value::int(123), Value::bool(true), Value::unit()],
            string_table: vec![],
            object_table: vec![],
            sender_actor: 7,
            sender_node: transport_a.node_id(),
            priority: MessagePriority::Normal,
            trace_id: None,
        };
        transport_a.send(node_b_id, addr_b_actual, good.clone());

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while Instant::now() < deadline && received.is_empty() {
            received = transport_b.receive();
            if received.is_empty() {
                sleep(Duration::from_millis(50));
            }
        }

        assert!(
            received.iter().any(|p| p.packet == good),
            "scalar payload must be delivered, got: {:?}",
            received
        );

        transport_a.shutdown();
        transport_b.shutdown();
    }

    // ------------------------------------------------------------------
    // 10. Sequence numbers increment
    // ------------------------------------------------------------------
    #[test]
    #[cfg(feature = "tcp")]
    fn test_transport_sequence_numbers() {
        let addr_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_a = TcpTransport::bind(
            addr_a,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_b = TcpTransport::bind(
            addr_b,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b_actual = transport_b.listen_addr();
        let node_b_id = transport_b.node_id();

        transport_a.connect(node_b_id, addr_b_actual).unwrap();
        sleep(Duration::from_millis(100));

        // The sender thread stamps each packet with a monotonic sequence
        // number in the wire header. Observe it on the receiving side —
        // there is no transport-level counter left to inspect.
        transport_a.send(node_b_id, addr_b_actual, Packet::Ack { packet_seq: 1 });
        transport_a.send(node_b_id, addr_b_actual, Packet::Ack { packet_seq: 2 });
        transport_a.send(node_b_id, addr_b_actual, Packet::Ack { packet_seq: 3 });

        let mut seqs = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while seqs.len() < 3 && Instant::now() < deadline {
            for p in transport_b.receive() {
                if matches!(p.packet, Packet::Ack { .. }) {
                    seqs.push(p.seq);
                }
            }
            if seqs.len() < 3 {
                sleep(Duration::from_millis(20));
            }
        }

        assert_eq!(
            seqs,
            vec![1, 2, 3],
            "wire sequence numbers must be monotonic per packet"
        );

        transport_a.shutdown();
        transport_b.shutdown();
    }

    // ------------------------------------------------------------------
    // 11. SpawnRequest / SpawnResponse roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_spawn_roundtrip() {
        let req = Packet::SpawnRequest {
            request_id: 12345,
            behavior_name: "Counter".into(),
            content_hash: None,
            initial_state: vec![
                ("count".into(), Value::int(0)),
                ("name".into(), Value::string(42)),
            ],
            bytecode: None,
        };
        let bytes = req.to_bytes(99);
        let (seq, decoded) = Packet::from_bytes(&bytes).unwrap();
        assert_eq!(seq, 99);
        assert_eq!(decoded, req);

        let resp = Packet::SpawnResponse {
            request_id: 12345,
            actor_id: 999,
            success: true,
        };
        let bytes = resp.to_bytes(100);
        let (seq, decoded) = Packet::from_bytes(&bytes).unwrap();
        assert_eq!(seq, 100);
        assert_eq!(decoded, resp);
    }

    // ------------------------------------------------------------------
    // 12. Corrupt / garbage bytes are rejected
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_from_bytes_rejects_garbage() {
        assert!(Packet::from_bytes(b"").is_none());
        assert!(Packet::from_bytes(b"NUL").is_none());
        assert!(Packet::from_bytes(b"XXXX\x00\x00\x00\x00\x00\x00\x00\x00\x00").is_none());
        assert!(Packet::from_bytes(b"NUL0\xFF\x00\x00\x00\x00\x00\x00\x00\x00").is_none());
    }

    // ------------------------------------------------------------------
    // 13. Ack roundtrip
    // ------------------------------------------------------------------
    #[test]
    fn test_packet_ack_roundtrip() {
        let packet = Packet::Ack {
            packet_seq: 0xCAFE_BABE,
        };
        let bytes = packet.to_bytes(42);
        let (seq, decoded) = Packet::from_bytes(&bytes).unwrap();
        assert_eq!(seq, 42);
        assert_eq!(decoded, packet);
    }

    // ------------------------------------------------------------------
    // 14. Disconnect removes connection
    // ------------------------------------------------------------------
    #[test]
    #[cfg(feature = "tcp")]
    fn test_transport_disconnect() {
        let addr_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_a = TcpTransport::bind(
            addr_a,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let addr_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let mut transport_b = TcpTransport::bind(
            addr_b,
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();

        let node_b_id = transport_b.node_id();

        transport_a
            .connect(node_b_id, transport_b.listen_addr())
            .unwrap();
        sleep(Duration::from_millis(100));

        assert!(transport_a.connection_count() >= 1);
        transport_a.disconnect(node_b_id);
        assert_eq!(transport_a.connection_count(), 0);

        transport_a.shutdown();
        transport_b.shutdown();
    }

    // -- D7c packet round-trips (RFC 0014 §6) ----------------------------

    #[test]
    fn test_packet_node_goodbye_roundtrip() {
        let packet = Packet::NodeGoodbye {
            node_id: NodeId(0xDEAD_BEEF_0000_0001),
            durable: vec![(7, 1), (9, 3), (u64::MAX, u64::MAX)],
        };
        let bytes = packet.to_bytes(42);
        let (seq, decoded) = Packet::from_bytes(&bytes).expect("node goodbye roundtrip");
        assert_eq!(seq, 42);
        assert_eq!(decoded, packet);
    }

    #[test]
    fn test_packet_node_goodbye_rejects_truncated_payload() {
        let packet = Packet::NodeGoodbye {
            node_id: NodeId(1),
            durable: vec![(7, 1)],
        };
        let bytes = packet.to_bytes(1);
        let truncated = &bytes[..bytes.len() - 3];
        assert!(Packet::from_bytes(truncated).is_none());
    }

    #[test]
    fn test_packet_shadow_replicate_roundtrip() {
        let packet = Packet::ShadowReplicate {
            actor_id: 0x1111_2222_3333_4444,
            nbc_bytes: vec![0x4E, 0x55, 0x4C, 0x30, 1, 2, 3, 4],
            snapshot_json: br#"{"actor_id":7}"#.to_vec(),
            epoch: 5,
        };
        let bytes = packet.to_bytes(77);
        let (seq, decoded) = Packet::from_bytes(&bytes).expect("shadow replicate roundtrip");
        assert_eq!(seq, 77);
        assert_eq!(decoded, packet);
    }

    #[test]
    fn test_packet_shadow_replicate_rejects_truncated_payload() {
        let packet = Packet::ShadowReplicate {
            actor_id: 1,
            nbc_bytes: vec![1, 2, 3, 4, 5],
            snapshot_json: vec![9, 9, 9],
            epoch: 1,
        };
        let bytes = packet.to_bytes(1);
        let truncated = &bytes[..bytes.len() - 4];
        assert!(Packet::from_bytes(truncated).is_none());
    }
}
#[cfg(feature = "tcp")]
impl NetworkTransport for TcpTransport {
    fn connect(&mut self, node_id: NodeId, addr: std::net::SocketAddr) -> std::io::Result<()> {
        self.connect(node_id, addr)
    }
    fn send(&mut self, to_node: NodeId, to_addr: std::net::SocketAddr, packet: Packet) {
        self.send(to_node, to_addr, packet)
    }
    fn receive(&self) -> Vec<IncomingPacket> {
        self.receive()
    }
    fn node_id(&self) -> NodeId {
        self.node_id()
    }
    fn listen_addr(&self) -> std::net::SocketAddr {
        self.listen_addr()
    }
    fn disconnect(&mut self, node_id: NodeId) {
        self.disconnect(node_id)
    }
    fn shutdown(&mut self) {
        self.shutdown()
    }
    fn connection_count(&self) -> usize {
        self.connection_count()
    }
    fn connection_addr(&self, node_id: NodeId) -> Option<std::net::SocketAddr> {
        self.connection_addr(node_id)
    }
    fn set_partition(&mut self, peers: HashSet<NodeId>) {
        self.partition = peers;
    }
}

/// In-memory deterministic network transport for DST.
/// Replaces TCP with channel-based message passing for reproducible testing.
#[derive(Debug)]
pub struct DeterministicNetworkTransport {
    node_id: NodeId,
    listen_addr: SocketAddr,
    /// Channel for receiving packets.
    incoming_rx: mpsc::Receiver<IncomingPacket>,
    incoming_tx: mpsc::SyncSender<IncomingPacket>,
    /// Shared bus for connecting to other nodes: node_id -> (incoming_tx, outgoing_tx)
    shared_bus: Arc<
        parking_lot::Mutex<
            HashMap<
                NodeId,
                (
                    mpsc::SyncSender<IncomingPacket>,
                    mpsc::SyncSender<OutgoingPacket>,
                ),
            >,
        >,
    >,
    shutdown_flag: Arc<AtomicBool>,
    /// Peers whose outbound packets are silently dropped (simulated
    /// partition, see [`NetworkTransport::set_partition`]).
    partition: HashSet<NodeId>,
    /// Bounded-adjacent-reorder mode (see [`NetworkTransport::set_reorder`]):
    /// when enabled, packets to a given peer are delivered with
    /// consecutive pairs swapped — the receiver sees P2 before P1.
    reorder: bool,
    /// The held previous packet per target (reorder mode only).
    held: HashMap<NodeId, IncomingPacket>,
}

impl Clone for DeterministicNetworkTransport {
    fn clone(&self) -> Self {
        DeterministicNetworkTransport {
            node_id: self.node_id,
            listen_addr: self.listen_addr,
            incoming_rx: mpsc::sync_channel(CHANNEL_CAPACITY).1,
            incoming_tx: self.incoming_tx.clone(),
            shared_bus: self.shared_bus.clone(),
            shutdown_flag: self.shutdown_flag.clone(),
            partition: HashSet::new(),
            reorder: false,
            held: HashMap::new(),
        }
    }
}

impl DeterministicNetworkTransport {
    /// Create a new deterministic transport bound to the given address.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        Self::bind_with_bus(addr, bus)
    }

    /// Create a new deterministic transport sharing the given message bus.
    /// All transports sharing the same bus can deliver packets to each other.
    pub fn bind_with_bus(
        addr: SocketAddr,
        shared_bus: Arc<
            parking_lot::Mutex<
                HashMap<
                    NodeId,
                    (
                        mpsc::SyncSender<IncomingPacket>,
                        mpsc::SyncSender<OutgoingPacket>,
                    ),
                >,
            >,
        >,
    ) -> io::Result<Self> {
        let node_id = NodeId::new(&addr);
        let (incoming_tx, incoming_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        Ok(DeterministicNetworkTransport {
            node_id,
            listen_addr: addr,
            incoming_rx,
            incoming_tx,
            shared_bus,
            shutdown_flag,
            partition: HashSet::new(),
            reorder: false,
            held: HashMap::new(),
        })
    }

    /// Register this transport on the shared bus so other transports can send to it.
    pub fn register_on_bus(&self) {
        let (outgoing_tx, _) = mpsc::sync_channel(CHANNEL_CAPACITY);
        self.shared_bus
            .lock()
            .insert(self.node_id, (self.incoming_tx.clone(), outgoing_tx));
    }

    /// Get the incoming sender for a target node.
    fn get_incoming_sender(&self, target: NodeId) -> Option<mpsc::SyncSender<IncomingPacket>> {
        self.shared_bus
            .lock()
            .get(&target)
            .map(|(tx, _)| tx.clone())
    }
}

impl NetworkTransport for DeterministicNetworkTransport {
    fn connect(&mut self, node_id: NodeId, _addr: SocketAddr) -> io::Result<()> {
        let _ = self.get_incoming_sender(node_id);
        Ok(())
    }

    fn send(&mut self, to_node: NodeId, _to_addr: SocketAddr, packet: Packet) {
        // Simulated partition: silently drop (see set_partition).
        if self.partition.contains(&to_node) {
            return;
        }
        if let Some(sender) = self.get_incoming_sender(to_node) {
            let incoming = IncomingPacket {
                from_node: self.node_id,
                seq: 0,
                packet,
            };
            if self.reorder {
                // Bounded adjacent reorder: hold the first packet of each
                // pair; when the second arrives, deliver the NEW one first
                // and then the held one — the receiver sees P2 before P1.
                // Deterministic (per-pair state, no RNG); nothing is lost
                // or duplicated, only delayed one slot. `flush_held`
                // delivers the odd tail at the end of the sender's turn.
                if let Some(held) = self.held.remove(&to_node) {
                    let _ = sender.try_send(incoming);
                    let _ = sender.try_send(held);
                } else {
                    self.held.insert(to_node, incoming);
                }
            } else {
                let _ = sender.try_send(incoming);
            }
        }
    }

    fn flush_held(&mut self) {
        // Drain into an owned Vec first: `held` is a field of `self`, and
        // `get_incoming_sender` borrows `self` — draining while borrowing
        // immutably would conflict.
        let held: Vec<(NodeId, IncomingPacket)> = self.held.drain().collect();
        for (to, pkt) in held {
            if let Some(sender) = self.get_incoming_sender(to) {
                let _ = sender.try_send(pkt);
            }
        }
    }

    fn set_reorder(&mut self, enabled: bool) {
        self.reorder = enabled;
        if !enabled {
            self.held.clear();
        }
    }

    fn receive(&self) -> Vec<IncomingPacket> {
        let mut packets = Vec::new();
        while let Ok(pkt) = self.incoming_rx.try_recv() {
            packets.push(pkt);
        }
        packets
    }

    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    fn disconnect(&mut self, _node_id: NodeId) {}

    fn shutdown(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
    }

    fn connection_count(&self) -> usize {
        self.shared_bus.lock().len()
    }

    fn connection_addr(&self, node_id: NodeId) -> Option<SocketAddr> {
        if self.shared_bus.lock().contains_key(&node_id) {
            Some(self.listen_addr)
        } else {
            None
        }
    }
    fn set_partition(&mut self, peers: HashSet<NodeId>) {
        self.partition = peers;
    }
}

#[cfg(feature = "tls")]
mod tls_provider {
    use crate::backends::{
        ClientTlsConfig, DefaultTlsProvider, ServerTlsConfig, TlsProvider, TlsStream,
    };
    use parking_lot::Mutex;
    use rustls::{ClientConfig, ServerConfig};
    use std::io;
    #[cfg(feature = "tcp")]
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;

    /// rustls-backed server TLS configuration.
    pub struct RustlsServerConfig {
        config: Arc<ServerConfig>,
    }
    impl ServerTlsConfig for RustlsServerConfig {}

    /// rustls-backed client TLS configuration.
    pub struct RustlsClientConfig {
        config: Arc<ClientConfig>,
    }

    impl ClientTlsConfig for RustlsClientConfig {}

    /// rustls-backed TLS stream wrapper.
    pub enum RustlsStream {
        Server(Arc<Mutex<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>>),
        Client(Arc<Mutex<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>>),
    }

    impl Read for RustlsStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self {
                RustlsStream::Server(s) => s.lock().read(buf),
                RustlsStream::Client(s) => s.lock().read(buf),
            }
        }
    }

    impl Write for RustlsStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self {
                RustlsStream::Server(s) => s.lock().write(buf),
                RustlsStream::Client(s) => s.lock().write(buf),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            match self {
                RustlsStream::Server(s) => s.lock().flush(),
                RustlsStream::Client(s) => s.lock().flush(),
            }
        }
    }

    impl TlsStream for RustlsStream {
        fn peer_certificates(&self) -> Option<Vec<Vec<u8>>> {
            match self {
                RustlsStream::Server(s) => {
                    let locked = s.lock();
                    locked
                        .conn
                        .peer_certificates()
                        .map(|c| c.iter().map(|cert| cert.to_vec()).collect())
                }
                RustlsStream::Client(s) => {
                    let locked = s.lock();
                    locked
                        .conn
                        .peer_certificates()
                        .map(|c| c.iter().map(|cert| cert.to_vec()).collect())
                }
            }
        }
    }

    impl TlsProvider for DefaultTlsProvider {
        fn server_config(&self) -> io::Result<Box<dyn ServerTlsConfig>> {
            // This is a placeholder; actual TLS config is built from TlsConfig
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Use TlsConfig::server_config() directly",
            ))
        }

        fn client_config(&self) -> io::Result<Box<dyn ClientTlsConfig>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Use TlsConfig::client_config() directly",
            ))
        }

        fn wrap_server_stream(
            &self,
            stream: TcpStream,
            config: Box<dyn ServerTlsConfig>,
        ) -> io::Result<Box<dyn TlsStream>> {
            let any_config = config as Box<dyn std::any::Any>;
            let rustls_config = any_config.downcast::<RustlsServerConfig>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "Expected RustlsServerConfig")
            })?;
            let conn = rustls::ServerConnection::new(Arc::clone(&rustls_config.config))
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            Ok(Box::new(RustlsStream::Server(Arc::new(Mutex::new(
                rustls::StreamOwned::new(conn, stream),
            )))))
        }

        fn wrap_client_stream(
            &self,
            stream: TcpStream,
            config: Box<dyn ClientTlsConfig>,
        ) -> io::Result<Box<dyn TlsStream>> {
            let any_config = config as Box<dyn std::any::Any>;
            let rustls_config = any_config.downcast::<RustlsClientConfig>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "Expected RustlsClientConfig")
            })?;
            let conn = rustls::ClientConnection::new(
                Arc::clone(&rustls_config.config),
                rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            )
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            Ok(Box::new(RustlsStream::Client(Arc::new(Mutex::new(
                rustls::StreamOwned::new(conn, stream),
            )))))
        }
    }
}
