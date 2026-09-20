# Nulang Threat Model

> Status: Draft — reflects current architecture as of 2026-09-19.  
> Audience: security reviewers, operators, and contributors.  
> Review cadence: every time a new network surface, FFI binding, sandbox boundary, or authority boundary is added.

## 1. Scope & Assumptions

### In scope
- The distributed actor runtime (TCP wire protocol, cluster membership, CRDT sync)
- The WASM backend (guest/host boundary, component-model sandbox)
- The FFI layer (`libloading` dynamic libraries, C ABI)
- The bytecode VM and JIT compiler (memory safety, sandbox escape)
- The persistence layer (SQLite, libsql/Turso, CRDT storage)
- The LSP server and REPL (input parsing, command injection)
- The package manager (`nula`) and registry client (supply chain, manifest parsing)
- External-resource authority (`AuthorityGrant` / `AuthorityManifest`) and actor spawn delegation

### Out of scope
- Security of the Rust toolchain, Linux kernel, or external Wasmtime/Cranelift dependencies (we rely on upstream CVE monitoring and `cargo audit` in CI)
- Physical security of the host machine
- Network-layer DDoS (handled by infrastructure, not Nulang runtime)

### Key assumptions
- **Threat actor**: an attacker who can send network traffic, craft malicious `.nula` source files, or supply compromised native libraries.
- **Trust boundary**: actors in the same process share an address space. Actor isolation is a compiler/runtime semantic boundary, not hardware isolation.
- **Operator responsibility**: TLS certificates for mutual-TLS distribution are managed by the operator, not by Nulang.

## 2. Threat Catalogue

### 2.1 Distributed Runtime (High)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| D1 | **Node impersonation** — attacker spoofs another node's ID in the NUL0 handshake | `TlsConfig::MutualTls` enforces client certs. Node IDs are validated against the TLS identity. | Partial: TLS config exists but is not CLI-wired yet. |
| D2 | **Wire-protocol tampering** — attacker modifies length-prefixed frames in transit | TLS encrypts the TCP stream. Big-endian frame encoding is deterministic, so truncation is detectable. | Partial: TLS available but not default. |
| D3 | **Split-brain** — network partition causes two clusters to diverge | `StaticQuorumResolver` (configurable via `set_cluster_config`) requires a quorum to accept membership changes. | Available: operator must configure expected node count. |
| D4 | **CRDT delta flooding** — malicious node sends oversized delta-sync packets | `sync_crdts_delta` limits delta batches to changed entries only. Full sync is rate-limited by `CRDT_FULL_SYNC_INTERVAL` (16 rounds). | Implemented. |
| D5 | **Remote spawn abuse** — attacker spawns arbitrary behaviors on remote nodes | `register_spawnable_behavior` is required; unknown names return `SpawnResponse{success:false}`. | Implemented for behavior allowlisting; this is separate from external-resource authority. |
| D6 | **Gossip amplification** — forged gossip packets propagate false membership | `ClusterState::merge_membership` only applies higher-incarnation entries; equal incarnation only refreshes heartbeat. | Implemented. |
| D7 | **Mailbox overflow** — queued messages consume unbounded memory | `Mailbox` supports an optional capacity for normal/bulk traffic and rejects over-capacity pushes with explicit backpressure. System-priority messages bypass that capacity so supervision/monitor traffic cannot be starved. | Partial: bounded application traffic is implemented, but the system lane means capacity is not a hard total-memory bound. |
| D8 | **Protocol confusion** — a remote actor ID accepts a message whose sender assumes a different behavior schema/version | Remote actor references are operational IDs today; RFC 0019 requires a first-class `ProtocolId` compatibility boundary. | Open high-priority risk. |

### 2.2 External Authority (High)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| A1 | **Spawned actor receives unintended filesystem/network/secret authority** | Typed `AuthorityGrant` / `AuthorityManifest` structures exist and exact-subset delegation is fail-closed. | Partial: RFC 0019 documents remaining parser → AST/HIR/MIR → bytecode → spawn-callback plumbing gaps. Do not treat reference capabilities as an end-to-end sandbox yet. |
| A2 | **String-based authority token confusion** | Canonical parsing converts legacy tokens to structural grants and rejects malformed tokens. | Migration in progress; security-sensitive paths should consume structural grants directly. |
| A3 | **Authority escalation during delegation** | `AuthorityManifest::is_subset_of` uses exact grant subset semantics rather than unsafe string-prefix attenuation. | Implemented at the manifest boundary; end-to-end spawn propagation remains incomplete. |

### 2.3 FFI & Native Code (Critical)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| F1 | **Arbitrary code execution via `dlopen`** | `FfiPolicy::Allowlist` restricts which libraries may be loaded. `--ffi-sandbox` + `--ffi-allow <LIB>` gates this at startup. | Implemented. |
| F2 | **Symbol squatting** — malicious library exports symbols expected by another library | `FfiRegistry` keys functions by `(library, symbol)` pair, not symbol alone. | Implemented. |
| F3 | **Use-after-free in C callbacks** | `OrcaGc` tracks foreign references. `free_object` releases slot references when containers are freed. | Implemented. |
| F4 | **Type confusion in FFI marshalling** | `Signature` types are checked at call time. `CType` is explicit (i64, f64, ptr, etc.). | Implemented. |

### 2.4 WASM Backend (High)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| W1 | **Sandbox escape via host imports** | Wasmtime `Linker` limits imports to explicitly-wrapped functions. Guard pages contain linear memory according to the configured Wasmtime runtime. | Implemented for the current host boundary. |
| W2 | **Capability escalation in component model** | A component authority/capability gate is planned for host imports. | Not yet implemented. |
| W3 | **Infinite loop in guest** | Wasmtime fuel metering or an execution timeout is required for untrusted execution. | Not yet implemented. |
| W4 | **AOT compilation of untrusted wasm** | `wasmtime compile` produces native artifacts intended to be loaded only by the trusted runtime. | Operational restriction; untrusted-input hardening is incomplete. |

### 2.5 VM & JIT (High)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| V1 | **JIT code injection** | JIT compilation is from compiler-produced MIR/bytecode, not arbitrary machine code. | Implemented boundary; compiler/JIT correctness remains part of the trusted computing base. |
| V2 | **Type confusion in JIT** | `typed_compiler` removes tag guards only when `TypeMetadata` proves register types and otherwise falls back to guarded/scalar paths. | Implemented. |
| V3 | **Division-by-zero in JIT** | Integer division/modulo use runtime helpers rather than unchecked native division. | Implemented. |
| V4 | **SIMD out-of-bounds access** | SIMD lowering is constrained by analyzer-proved loop shapes and scalar remainder handling. | Implemented for supported SIMD regions; regression testing remains required. |

### 2.6 Persistence & Storage (Medium)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| P1 | **SQL injection in persistence backends** | Serialized values are passed through backend APIs rather than building executable SQL from user value strings. | Implemented for current persistence paths. |
| P2 | **CRDT tombstone accumulation** | OR-set variants expose tombstone GC below a configured watermark. | Implemented; correctness depends on choosing a safe watermark policy. |
| P3 | **Data exfiltration via checkpoint serialization** | Checkpoint code serializes actor-owned state according to runtime persistence rules. | Implemented boundary; new pointer/host-handle types require review before being made durable. |
| P4 | **Duplicate external side effects during workflow replay** | RFC 0019 requires durable effects to be replay-safe, idempotent with a stable operation key, explicitly compensatable, or forbidden. | Semantic contract not fully closed; high-priority correctness/security work. |

### 2.7 Package Manager & Supply Chain (Medium)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| S1 | **Dependency confusion** | `nula` dependencies can be pinned to explicit paths/git sources/registry identities rather than executing package manifests as code. | Implemented mechanisms; registry policy still matters. |
| S2 | **Manifest tampering / code execution during parsing** | `Nulang.toml` is data parsed by the TOML parser, not an executable build script. | Implemented. |
| S3 | **Language version pin bypass** | Package preparation validates the declared language version against the runtime/compiler compatibility rules. | Implemented. |
| S4 | **Registry impersonation** | Registry publishing uses bearer authentication over configured HTTPS endpoints. | Implemented mechanism; endpoint/certificate trust remains an operator concern. |

## 3. Attack Scenarios

### Scenario A: Malicious remote actor
An attacker gains access to a single node and sends a crafted `ActorMessage`
to a remote actor.

- **Payload**: wire-safety checks reject process-local pointer-like values that
  cannot safely cross the current transport boundary.
- **String injection**: strings travel as data; they become executable only if
  application logic explicitly interprets them as such.
- **Behavior/protocol confusion**: behavior names are resolved at the target,
  but actor references do not yet carry RFC 0019 protocol identity. Treat
  cross-version distributed dispatch as an open compatibility/security
  boundary rather than a proven invariant.

### Scenario B: Compromised native library
An attacker replaces a `.so` file that a Nulang program loads via FFI.

- **Mitigation**: `--ffi-sandbox` restricts loading to an explicit allowlist.
  Native code remains inside the trusted computing base once loaded.
- **Detection**: dependency CVE scanning does not detect a malicious local
  library replacement. Operators should additionally use filesystem/package
  integrity controls for production native libraries.

### Scenario C: Untrusted WASM module
An attacker uploads a `.wasm` module to a Nulang service that runs user code.

- **Mitigation**: Wasmtime isolates guest linear memory and host imports are
  explicitly linked.
- **Residual risk**: resource metering and the component-model authority gate
  are not complete. Do not expose arbitrary untrusted execution as a hardened
  multi-tenant sandbox until those controls are in place.

### Scenario D: Authority confusion
A program uses reference-capability syntax and assumes that this also limits
filesystem/network/secret access.

- **Mitigation direction**: reference capabilities and external authority are
  distinct concepts. Structural `AuthorityGrant` values are the intended
  security boundary.
- **Residual risk**: RFC 0019 records remaining spawn-authority plumbing gaps.
  Until those are closed and tested end-to-end, operators must not treat actor
  reference capabilities as a complete external-resource sandbox.

## 4. Security Checklist for Operators

- [ ] Enable mutual TLS for production clusters where supported; plaintext distribution is not an acceptable trust boundary.
- [ ] Configure quorum/split-brain policy appropriate to the expected cluster size.
- [ ] Configure finite mailbox capacities for untrusted normal/bulk producers and monitor the system-priority lane separately.
- [ ] Use `--ffi-sandbox --ffi-allow /path/to/lib.so` where native FFI is required; prefer disabling FFI entirely when it is not.
- [ ] Do not treat reference capabilities as external-resource authorization until RFC 0019 authority propagation is complete.
- [ ] Monitor mailbox depths, system-message rates, actor reductions, persistence growth, and restart storms.
- [ ] Run dependency/security audit jobs continuously and review unsafe-runtime changes as security-sensitive.
- [ ] Verify `.nbc` provenance/compatibility metadata before executing externally supplied artifacts.
- [ ] Pin the intended language/runtime compatibility version in package manifests and test upgrades before deployment.

## 5. Open Risks & Follow-up Work

| Risk | Priority | Tracking |
|---|---|---|
| End-to-end typed spawn authority propagation/enforcement | Critical | RFC 0019 Semantic Closure |
| Protocol-typed `ActorRef<P>` / `ProtocolId` distributed compatibility | High | RFC 0019 Semantic Closure |
| Durable external-effect idempotency/commit crash-window semantics | High | RFC 0019 Semantic Closure |
| WASM component authority gate + resource metering | High | WASM security work |
| System-priority mailbox flood is outside normal/bulk capacity | High | Runtime backpressure / DoS hardening |
| OTLP trace/metric export for security-event monitoring | Medium | Observability |
| Windows CI/runtime hardening | Medium | Portability |
| Gossip amplification under Byzantine majority | Low | Research / threat analysis |

## 6. References

- `src/runtime/network.rs` — NUL0 wire protocol
- `src/runtime/cluster.rs` — `ClusterState`, quorum/split-brain handling
- `src/runtime/mailbox.rs` — priority lanes, capacity, backpressure
- `src/authority.rs` — structural authority grants and manifests
- `src/authority_runtime.rs` / `src/authority_host.rs` — runtime authority boundaries
- `src/ffi/native.rs` — `FfiPolicy`, `FfiRegistry`
- `src/mir_wasm.rs` — WASM lowering
- `src/jit/typed_compiler.rs` — typed JIT metadata and guarded optimization
- `src/format/constants.rs` — language/format version constants
- `RFC/0019-semantic-closure.md` — current semantic-security closure plan
- `GOVERNANCE.md` — stability tiers and RFC process
- `SPEC2.md` — language/runtime specification
