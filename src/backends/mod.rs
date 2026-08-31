//! Backend trait boundaries — the longevity layer that decouples the
//! language from its transient dependencies.
//!
//! Every backend (JIT, WASM, storage, transport) is accessed through a trait
//! defined here. The core language (`src/vm.rs`, `src/runtime/`, `src/main.rs`)
//! talks only to these traits. Concrete implementations live in their existing
//! modules (`src/jit/`, `src/mir_wasm.rs`, `src/wasm_runtime.rs`,
//! `src/runtime/persistence.rs`, `src/runtime/network.rs`) and are selected at
//! link time via feature flags.
//!
//! This means a 2125 Nulang runtime can swap Cranelift for whatever codegen
//! exists then, Wasmtime for whatever WASM runtime exists then, and
//! quinn/rustls for whatever transport exists then — without touching
//! `src/vm.rs`, `src/bytecode.rs`, or any user program.
//!
//! # Current status
//!
//! - [`StorageBackend`] — blanket-impl'd over [`crate::runtime::PersistenceStore`].
//! - [`JitBackend`] — implemented by [`crate::jit::JitSession`]; the VM
//!   holds `Option<Box<dyn JitBackend>>`.  **Wired.**
//! - [`WasmBackend`] — implemented by [`DefaultWasmBackend`]; `main.rs`
//!   uses the trait.  **Wired.**
//! - [`Transport`] — blanket-impl'd over [`crate::runtime::NetworkTransport`].
//!   **Wired.**
//! - [`CryptoProvider`] — implemented by [`DefaultCryptoProvider`]; field
//!   on [`crate::runtime::Runtime`] with `hash_bytes`/`random_bytes` helpers.
//!   **Wired.** (identity.rs uses direct ed25519 for ActorId key generation.)
//! - [`HttpProvider`] — implemented by [`ReqwestHttpProvider`]; field on
//!   [`crate::runtime::Runtime`] with `http_post_json`/`http_get` helpers.
//!   **Wired.**
//! - [`ForeignInterop`] — implemented by [`DefaultForeignInterop`] (PyO3,
//!   feature `python`).  **Wired.**

use crate::bytecode::CodeModule;
use crate::mir::Module as MirModule;
use crate::types::NuResult;
use crate::vm::Value;

// ---------------------------------------------------------------------------
// Storage backend — already exists as PersistenceStore, re-exported here
// ---------------------------------------------------------------------------

/// The storage backend trait. This is the single point through which the
/// runtime accesses durable storage. Concrete impls: `MemoryStore`,
/// `JsonFileStore`, `SqliteStore` (feature `sqlite`).
///
/// This is a re-export of [`crate::runtime::PersistenceStore`] — storage was
/// already behind a trait. This alias makes the boundary discoverable from
/// one place.
pub trait StorageBackend: crate::runtime::PersistenceStore {}
impl<T: crate::runtime::PersistenceStore> StorageBackend for T {}

// ---------------------------------------------------------------------------
// JIT backend — the interface for register-VM JIT compilers
// ---------------------------------------------------------------------------

/// Action taken by the tiered execution system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TieredAction {
    /// The JIT could not run; fall back to the interpreter.
    Interpret,
    /// JIT-compiled code was executed; advance the PC.
    RanJit,
    /// The region was SIMD-vectorized, compiled, and executed.
    /// (Unused while the SIMD path is gated off; kept for API stability.)
    CompiledSimdAndRan,
}

/// A JIT backend compiles hot bytecode regions into native code for faster
/// execution. The default implementation uses Cranelift (`src/jit/`). A future
/// runtime could implement this trait with LLVM, GCC JIT, or whatever
/// codegen exists in 2125.
pub trait JitBackend {
    /// Whether a region at `(module_idx, pc)` has already been compiled.
    fn is_compiled(&self, module_idx: usize, pc: usize) -> bool;

    /// Record one interpretation and return `true` when the region is hot.
    fn record_and_check_hot(&mut self, module_idx: usize, pc: usize) -> bool;

    /// Fast combined per-step probe: return `true` when the region at
    /// `(module_idx, pc)` should execute now — because it is already
    /// compiled, or it just became hot (recording one interpretation when
    /// not compiled). This is the ONLY per-step probe the interpreter pays
    /// when the JIT is enabled, so it must be a single dyn-dispatch call
    /// (the default delegates to `is_compiled` + `record_and_check_hot` for
    /// alternative backends; `JitSession` overrides it with inlined logic).
    fn probe_and_maybe_hot(&mut self, module_idx: usize, pc: usize) -> bool {
        if self.is_compiled(module_idx, pc) {
            true
        } else {
            self.record_and_check_hot(module_idx, pc)
        }
    }

    /// Number of bytecode instructions in the compiled region at `(module_idx, pc)`.
    fn compiled_region_len(&self, module_idx: usize, pc: usize) -> Option<usize>;

    /// Number of regions compiled (scalar path).
    fn compiled_count(&self) -> usize;

    /// Number of regions compiled through the type-directed path.
    fn typed_compiled_count(&self) -> usize;

    /// Reset hot counters.
    fn reset_hot_counters(&mut self);

    /// Wave E2 loop-OSR: compile the loop region starting at `pc` — a loop
    /// header reached by a hot interpreter back-edge — WITHOUT executing it.
    /// Returns `true` when a compiled region exists at `pc` afterwards
    /// (already compiled, or compiled now). Compilation failure or an
    /// un-compilable region (e.g. an unsupported opcode inside the loop)
    /// returns `false`; the caller then deopts by staying in the interpreter.
    /// Default: no OSR support.
    fn osr_compile_loop(&mut self, module_idx: usize, pc: usize, module: &CodeModule) -> bool {
        let _ = (module_idx, pc, module);
        false
    }

    /// Execute one tiered step: if the region at `pc` is compiled, run it;
    /// if hot, compile then run; otherwise record and return `Interpret`.
    fn tiered_execute_step_typed(
        &mut self,
        module_idx: usize,
        pc: usize,
        module: &CodeModule,
        regs: &mut [u64; 256],
        constants: &[u64],
    ) -> TieredAction;
}

// ---------------------------------------------------------------------------
// WASM backend — the interface for MIR→WASM compilers + host runtimes
// ---------------------------------------------------------------------------

/// A WASM backend compiles MIR to WASM bytes and provides a host
/// runtime to execute it. The default implementation uses `wasm-encoder` +
/// `wasmtime` (`src/mir_wasm.rs` + `src/wasm_runtime.rs`, feature
/// `wasm-backend`). A future runtime could implement this trait with a
/// different WASM compiler or host.
pub trait WasmBackend: Send {
    /// Compile a MIR module to WASM bytes.
    fn compile(&mut self, module: &MirModule, name: &str) -> NuResult<Vec<u8>>;

    /// Run a compiled WASM module. Returns the tagged program result.
    fn run(&mut self, wasm: &[u8]) -> NuResult<Value>;
}

// ---------------------------------------------------------------------------
// Default WASM backend impl — delegates to mir_wasm + wasm_runtime
// ---------------------------------------------------------------------------

/// The default WASM backend: compiles via `mir_wasm::WasmBackend`,
/// runs via `wasm_runtime::WasmRuntime`.
#[cfg(feature = "wasm-backend")]
pub struct DefaultWasmBackend;

#[cfg(feature = "wasm-backend")]
impl WasmBackend for DefaultWasmBackend {
    fn compile(&mut self, module: &MirModule, name: &str) -> NuResult<Vec<u8>> {
        crate::mir_wasm::WasmBackend::new().compile(module, name)
    }

    fn run(&mut self, wasm: &[u8]) -> NuResult<Value> {
        crate::wasm_runtime::WasmRuntime::new(wasm, None)?.run()
    }
}

// ---------------------------------------------------------------------------
// HTTP provider — the interface for outbound HTTP requests
// ---------------------------------------------------------------------------

/// An HTTP provider makes outbound HTTP requests. The default implementation
/// uses `reqwest`. A future runtime could implement this with hyper, curl,
/// or whatever HTTP client exists in 2125.
pub trait HttpProvider: Send + Sync {
    /// Perform a synchronous POST with a JSON body and return the response body.
    fn post_json(&self, url: &str, body: &str) -> Result<String, String>;

    /// Perform a synchronous GET and return the response body.
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Default HTTP provider backed by `reqwest` (requires `ai-runtime` feature).
#[cfg(any(feature = "ai-runtime", feature = "http-client"))]
#[derive(Debug, Clone)]
pub struct ReqwestHttpProvider {
    client: reqwest::Client,
}

#[cfg(any(feature = "ai-runtime", feature = "http-client"))]
impl ReqwestHttpProvider {
    /// Create a new reqwest-backed HTTP provider with a default timeout.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        ReqwestHttpProvider { client }
    }
}

#[cfg(any(feature = "ai-runtime", feature = "http-client"))]
impl Default for ReqwestHttpProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(feature = "ai-runtime", feature = "http-client"))]
impl HttpProvider for ReqwestHttpProvider {
    fn post_json(&self, url: &str, body: &str) -> Result<String, String> {
        let client = self.client.clone();
        tokio::runtime::Handle::try_current()
            .map_err(|_| "no Tokio runtime available".to_string())?
            .block_on(async {
                client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(body.to_string())
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .text()
                    .await
                    .map_err(|e| e.to_string())
            })
    }

    fn get(&self, url: &str) -> Result<String, String> {
        let client = self.client.clone();
        tokio::runtime::Handle::try_current()
            .map_err(|_| "no Tokio runtime available".to_string())?
            .block_on(async {
                client
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .text()
                    .await
                    .map_err(|e| e.to_string())
            })
    }
}

// ---------------------------------------------------------------------------
// Transport backend — the interface for network transports
// ---------------------------------------------------------------------------

/// A transport backend provides point-to-point packet delivery between
// cluster nodes. The default implementation uses TCP
// (`src/runtime/network.rs`). A future runtime could implement this with
// QUIC, UDP, or whatever transport exists in 2125.
//
// This trait mirrors the existing [`crate::runtime::NetworkTransport`] trait
// — network transport was already behind a trait. This re-export makes the
// boundary discoverable from one place.
pub trait Transport: Send {
    fn connect(
        &mut self,
        node_id: crate::runtime::NodeId,
        addr: std::net::SocketAddr,
    ) -> std::io::Result<()>;
    fn send(
        &mut self,
        to_node: crate::runtime::NodeId,
        to_addr: std::net::SocketAddr,
        packet: crate::runtime::Packet,
    );
    fn receive(&self) -> Vec<crate::runtime::IncomingPacket>;
    fn node_id(&self) -> crate::runtime::NodeId;
    fn listen_addr(&self) -> std::net::SocketAddr;
    fn disconnect(&mut self, node_id: crate::runtime::NodeId);
    fn shutdown(&mut self);
    fn connection_count(&self) -> usize;
    fn connection_addr(&self, node_id: crate::runtime::NodeId) -> Option<std::net::SocketAddr>;
}

impl<T: crate::runtime::NetworkTransport> Transport for T {
    fn connect(
        &mut self,
        node_id: crate::runtime::NodeId,
        addr: std::net::SocketAddr,
    ) -> std::io::Result<()> {
        crate::runtime::NetworkTransport::connect(self, node_id, addr)
    }
    fn send(
        &mut self,
        to_node: crate::runtime::NodeId,
        to_addr: std::net::SocketAddr,
        packet: crate::runtime::Packet,
    ) {
        crate::runtime::NetworkTransport::send(self, to_node, to_addr, packet)
    }
    fn receive(&self) -> Vec<crate::runtime::IncomingPacket> {
        crate::runtime::NetworkTransport::receive(self)
    }
    fn node_id(&self) -> crate::runtime::NodeId {
        crate::runtime::NetworkTransport::node_id(self)
    }
    fn listen_addr(&self) -> std::net::SocketAddr {
        crate::runtime::NetworkTransport::listen_addr(self)
    }
    fn disconnect(&mut self, node_id: crate::runtime::NodeId) {
        crate::runtime::NetworkTransport::disconnect(self, node_id)
    }
    fn shutdown(&mut self) {
        crate::runtime::NetworkTransport::shutdown(self)
    }
    fn connection_count(&self) -> usize {
        crate::runtime::NetworkTransport::connection_count(self)
    }
    fn connection_addr(&self, node_id: crate::runtime::NodeId) -> Option<std::net::SocketAddr> {
        crate::runtime::NetworkTransport::connection_addr(self, node_id)
    }
}

// ---------------------------------------------------------------------------
// TLS provider — the interface for TLS configuration and connection handling
// ---------------------------------------------------------------------------

/// A TLS provider supplies server/client TLS configuration and stream wrapping.
/// The default implementation uses `rustls` (`src/runtime/network.rs`).
/// A future runtime could implement this with a different TLS library
/// (e.g., OpenSSL, BoringSSL, or a post-quantum TLS implementation).
pub trait TlsProvider: Send + Sync {
    /// Build a server TLS configuration for accepting connections.
    fn server_config(&self) -> std::io::Result<Box<dyn ServerTlsConfig>>;

    /// Build a client TLS configuration for dialing connections.
    fn client_config(&self) -> std::io::Result<Box<dyn ClientTlsConfig>>;

    /// Wrap a raw TCP stream as a TLS server stream.
    fn wrap_server_stream(
        &self,
        stream: std::net::TcpStream,
        config: Box<dyn ServerTlsConfig>,
    ) -> std::io::Result<Box<dyn TlsStream>>;

    /// Wrap a raw TCP stream as a TLS client stream.
    fn wrap_client_stream(
        &self,
        stream: std::net::TcpStream,
        config: Box<dyn ClientTlsConfig>,
    ) -> std::io::Result<Box<dyn TlsStream>>;
}

/// Server-side TLS configuration.
pub trait ServerTlsConfig: Send + Sync + std::any::Any {}

/// Client-side TLS configuration.
pub trait ClientTlsConfig: Send + Sync + std::any::Any {}

/// A TLS-wrapped stream.
pub trait TlsStream: std::io::Read + std::io::Write + Send {
    /// Get the peer's certificate chain, if any.
    fn peer_certificates(&self) -> Option<Vec<Vec<u8>>>;
}

// ---------------------------------------------------------------------------
// Default TLS provider impl — rustls
// ---------------------------------------------------------------------------

/// The default TLS provider: rustls with PEM-based certificate handling.
pub struct DefaultTlsProvider;

impl DefaultTlsProvider {
    pub fn new() -> Self {
        DefaultTlsProvider
    }
}

impl Default for DefaultTlsProvider {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Crypto provider — the interface for cryptographic operations
// ---------------------------------------------------------------------------

/// A crypto provider supplies hashing, secure random, and optional signing.
/// The default implementation uses BLAKE3 + `getrandom` + `ed25519-dalek`
/// (`src/runtime/identity.rs`). A future runtime could implement
/// a hardware security module, a different hash function, or whatever
/// cryptographic primitives exist in 2125.
pub trait CryptoProvider: Send + Sync {
    /// Compute the BLAKE3-256 hash of `data` (32 bytes).
    /// (The algorithm is BLAKE3, not SHA-256 — the output length is 32 bytes.)
    fn hash(&self, data: &[u8]) -> [u8; 32];

    /// Fill `buf` with cryptographically secure random bytes.
    fn random_bytes(&self, buf: &mut [u8]);

    /// Sign `message` with the node's Ed25519 private key.
    /// Returns the 64-byte signature, or `None` if no key is configured.
    fn sign(&self, message: &[u8]) -> Option<[u8; 64]>;

    /// Verify an Ed25519 signature.  `public_key` is 32 bytes, `signature`
    /// is 64 bytes.  Returns `true` iff the signature is valid.
    fn verify(&self, public_key: &[u8], message: &[u8], signature: &[u8; 64]) -> bool;
}

// ---------------------------------------------------------------------------
// Foreign-interop backend — the interface for calling foreign code
// ---------------------------------------------------------------------------

/// A foreign-interop backend bridges the Nulang VM to an external language
/// runtime.  The default implementation uses PyO3 for Python interop
/// (`src/python/`, feature `python`).  A future runtime could implement
/// this with a JavaScript engine, a WASM component model host, or whatever
/// foreign runtime exists in 2125.
pub trait ForeignInterop: Send {
    /// Call a named foreign function with the given arguments.
    /// Returns the marshalled result on success, or an error string.
    fn call(&mut self, module: &str, function: &str, args: &[Value]) -> Result<Value, String>;

    /// Import a foreign module, making its exports available via `call`.
    /// Returns an error string if the module cannot be loaded.
    fn import(&mut self, name: &str) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Default Foreign-interop impl — Python via PyO3
// ---------------------------------------------------------------------------

/// The default foreign-interop backend: Python via PyO3 (`src/python/`,
/// feature `python`).
#[cfg(feature = "python")]
pub struct DefaultForeignInterop {
    bridge: crate::python::PyBridge,
    /// Cached module names → Python object ids, for `call` lookups.
    modules: std::collections::HashMap<String, crate::python::PythonObjectId>,
}

#[cfg(feature = "python")]
impl DefaultForeignInterop {
    pub fn new() -> Result<Self, String> {
        let bridge = crate::python::PyBridge::new();
        bridge.initialize()?;
        Ok(DefaultForeignInterop {
            bridge,
            modules: std::collections::HashMap::new(),
        })
    }
}

#[cfg(feature = "python")]
impl ForeignInterop for DefaultForeignInterop {
    fn import(&mut self, name: &str) -> Result<(), String> {
        let id = self.bridge.import_module(name)?;
        self.modules.insert(name.to_string(), id);
        Ok(())
    }

    fn call(&mut self, module: &str, function: &str, args: &[Value]) -> Result<Value, String> {
        let module_id = *self
            .modules
            .get(module)
            .ok_or_else(|| format!("module '{}' not imported", module))?;
        let func_id = self.bridge.get_attr(module_id, function)?;
        let py_args: Result<Vec<_>, String> = args
            .iter()
            .map(|v| crate::python::value_to_python_object_id(*v))
            .collect();
        let result_id = self.bridge.call(func_id, py_args?)?;
        crate::python::python_object_id_to_value(result_id)
    }
}

// ---------------------------------------------------------------------------
// Default Crypto provider impl — BLAKE3 + Ed25519
// ---------------------------------------------------------------------------

/// The default crypto provider: BLAKE3 hashing, `getrandom` CSPRNG,
/// and Ed25519 signatures via `ed25519-dalek`.
pub struct DefaultCryptoProvider {
    signing_key: Option<ed25519_dalek::SigningKey>,
}

impl DefaultCryptoProvider {
    /// Create a provider with no signing key.  `sign()` will return `None`.
    pub fn new() -> Self {
        DefaultCryptoProvider { signing_key: None }
    }

    /// Create a provider with a signing key.
    pub fn with_signing_key(key: ed25519_dalek::SigningKey) -> Self {
        DefaultCryptoProvider {
            signing_key: Some(key),
        }
    }
}

impl Default for DefaultCryptoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CryptoProvider for DefaultCryptoProvider {
    fn hash(&self, data: &[u8]) -> [u8; 32] {
        *blake3::hash(data).as_bytes()
    }

    fn random_bytes(&self, buf: &mut [u8]) {
        use rand_core::RngCore;
        rand_core::OsRng.fill_bytes(buf);
    }

    fn sign(&self, message: &[u8]) -> Option<[u8; 64]> {
        let key = self.signing_key.as_ref()?;
        use ed25519_dalek::Signer;
        let sig = key.sign(message);
        let mut out = [0u8; 64];
        out.copy_from_slice(&sig.to_bytes());
        Some(out)
    }

    fn verify(&self, public_key: &[u8], message: &[u8], signature: &[u8; 64]) -> bool {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let Ok(pk_bytes) = <[u8; 32]>::try_from(public_key) else {
            return false;
        };
        let vk = match VerifyingKey::from_bytes(&pk_bytes) {
            Ok(k) => k,
            Err(_) => return false,
        };
        let sig = Signature::from_bytes(signature);
        vk.verify(message, &sig).is_ok()
    }
}

// ---------------------------------------------------------------------------
// Factory functions — the only place concrete backend types are constructed
// ---------------------------------------------------------------------------

/// Create the default JIT backend (Cranelift via `JitSession`).
///
/// This is the **sole** call-site for `JitSession::new()` outside of tests.
/// The VM calls this factory rather than importing `JitSession` directly,
/// keeping the JIT implementation behind the `JitBackend` trait boundary.
pub fn create_default_jit() -> Option<Box<dyn JitBackend>> {
    crate::jit::JitSession::new().map(|j| Box::new(j) as Box<dyn JitBackend>)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_backend_is_persistence_store() {
        // StorageBackend is auto-implemented for any PersistenceStore.
        fn accepts_storage<S: StorageBackend>(_s: &S) {}
        fn accepts_persistence<P: crate::runtime::PersistenceStore>(p: &P) {
            accepts_storage(p);
        }
        let store = crate::runtime::MemoryStore::new();
        accepts_persistence(&store);
    }

    #[cfg(feature = "tcp")]
    #[test]
    fn test_transport_is_network_transport() {
        fn check_blanket<T: crate::runtime::NetworkTransport>() {
            fn _assert_trait_object(_: &dyn Transport) {}
        }
        check_blanket::<crate::runtime::TcpTransport>();
    }
    #[cfg(any(feature = "ai-runtime", feature = "http-client"))]
    #[test]
    fn test_http_provider_is_object_safe() {
        fn accepts_http(_h: &dyn HttpProvider) {
            // Verify trait object usage compiles (no runtime needed for type-check).
        }
        let provider = ReqwestHttpProvider::new();
        accepts_http(&provider);
    }

    #[test]
    fn test_default_crypto_provider_hash_and_random() {
        let cp = DefaultCryptoProvider::new();
        // hash is deterministic
        let h1 = cp.hash(b"hello");
        let h2 = cp.hash(b"hello");
        assert_eq!(h1, h2);
        assert_ne!(h1, cp.hash(b"world"));
        // random_bytes fills the buffer
        let mut buf = [0u8; 32];
        cp.random_bytes(&mut buf);
        assert!(
            buf.iter().any(|&b| b != 0),
            "random bytes should not be all zero"
        );
        // sign returns None without a key
        assert!(cp.sign(b"message").is_none());
    }

    #[test]
    fn test_default_crypto_provider_sign_verify() {
        use ed25519_dalek::SigningKey;
        use rand_core::OsRng;
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let verifying_key = signing_key.verifying_key();
        let cp = DefaultCryptoProvider::with_signing_key(signing_key);

        let msg = b"nulang test message";
        let sig = cp.sign(msg).expect("sign should succeed with key");
        let pk: [u8; 32] = verifying_key.to_bytes();
        assert!(cp.verify(&pk, msg, &sig));
        assert!(!cp.verify(&pk, b"wrong message", &sig));
    }

    #[cfg(feature = "python")]
    #[test]
    fn test_foreign_interop_python_roundtrip() {
        // Ensure Python is initialized (auto-initialize feature handles this).
        let _ = pyo3::Python::attach(|_py| ());

        let mut fi = DefaultForeignInterop::new().expect("failed to create DefaultForeignInterop");
        // Import the builtins module
        fi.import("builtins").expect("failed to import builtins");
        // Call abs(-42) → 42
        let result = fi
            .call("builtins", "abs", &[Value::int(-42)])
            .expect("failed to call abs");
        assert_eq!(result.as_int(), Some(42), "abs(-42) should return 42");
        // Call without importing first should fail
        let err = fi
            .call("nonexistent", "fn", &[])
            .expect_err("call without import should fail");
        assert!(
            err.contains("not imported"),
            "expected 'not imported' error"
        );
    }
}
