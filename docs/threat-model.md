# Nulang Threat Model

> Status: Draft — reflects current architecture as of 2026-08-08.  
> Audience: security reviewers, operators, and contributors.  
> Review cadence: every time a new network surface, FFI binding, or sandbox boundary is added.

## 1. Scope & Assumptions

### In scope
- The distributed actor runtime (TCP wire protocol, cluster membership, CRDT sync)
- The WASM backend (guest/host boundary, component-model sandbox)
- The FFI layer (`libloading` dynamic libraries, C ABI)
- The bytecode VM and JIT compiler (memory safety, sandbox escape)
- The persistence layer (SQLite, libsql/Turso, CRDT storage)
- The LSP server and REPL (input parsing, command injection)
- The package manager (`nula`) and registry client (supply chain, manifest parsing)

### Out of scope
- Security of the Rust toolchain, Linux kernel, or external Wasmtime/Cranelift dependencies (we rely on upstream CVE monitoring and `cargo audit` in CI)
- Physical security of the host machine
- Network-layer DDoS (handled by infrastructure, not Nulang runtime)

### Key assumptions
- **Threat actor**: an attacker who can send network traffic, craft malicious `.nula` source files, or supply compromised native libraries.
- **Trust boundary**: actors within the same runtime shard share an address space (cooperative, not hardware-isolated). Cross-shard messages are serialized and therefore bounded.
- **Operator responsibility**: TLS certificates for mutual-TLS distribution are managed by the operator, not by Nulang.

## 2. Threat Catalogue

### 2.1 Distributed Runtime (High)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| D1 | **Node impersonation** — attacker spoofs another node's ID in the NUL0 handshake | `TlsConfig::MutualTls` enforces client certs. Node IDs are validated against the TLS identity. | Partial: TLS config exists but is not CLI-wired yet. |
| D2 | **Wire-protocol tampering** — attacker modifies length-prefixed frames in transit | TLS encrypts the TCP stream. Big-endian frame encoding is deterministic, so truncation is detectable. | Partial: TLS available but not default. |
| D3 | **Split-brain** — network partition causes two clusters to diverge | `StaticQuorumResolver` (configurable via `set_cluster_config`) requires a quorum to accept membership changes. | Available: operator must configure expected node count. |
| D4 | **CRDT delta flooding** — malicious node sends oversized delta-sync packets | `sync_crdts_delta` limits delta batches to changed entries only. Full sync is rate-limited by `CRDT_FULL_SYNC_INTERVAL` (16 rounds). | Implemented. |
| D5 | **Remote spawn abuse** — attacker spawns arbitrary behaviors on remote nodes | `register_spawnable_behavior` is required; unknown names return `SpawnResponse{success:false}`. | Implemented. |
| D6 | **Gossip amplification** — forged gossip packets propagate false membership | `ClusterState::merge_membership` only applies higher-incarnation entries; equal incarnation only refreshes heartbeat. | Implemented. |
| D7 | **Mailbox overflow** — unbounded mailbox growth causes OOM | `Mailbox` is a `SegQueue` (unbounded). Current policy: never drop. Mitigation: per-turn reduction budget (1000 msgs) and actor GC. | Accepted risk: operators must monitor. |

### 2.2 FFI & Native Code (Critical)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| F1 | **Arbitrary code execution via `dlopen`** | `FfiPolicy::Allowlist` restricts which libraries may be loaded. `--ffi-sandbox` + `--ffi-allow <LIB>` gates this at startup. | Implemented. |
| F2 | **Symbol squatting** — malicious library exports symbols expected by another library | `FfiRegistry` keys functions by `(library, symbol)` pair, not symbol alone. | Implemented. |
| F3 | **Use-after-free in C callbacks** | `OrcaGc` tracks foreign references. `free_object` releases slot references when containers are freed. | Implemented. |
| F4 | **Type confusion in FFI marshalling** | `Signature` types are checked at call time. `CType` is explicit (i64, f64, ptr, etc.). | Implemented. |

### 2.3 WASM Backend (High)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| W1 | **Sandbox escape via host imports** | Wasmtime `Linker` limits imports to explicitly-wrapped functions. Guard pages (4GiB reserved, 128MiB guard) contain linear memory. | Implemented. |
| W2 | **Capability escalation in component model** | WASM component capability gate (TBD in P5b) will restrict which host capabilities a component may import. | Not yet implemented. |
| W3 | **Infinite loop in guest** | Wasmtime fuel metering (not yet wired) or execution timeout. | Not yet implemented. |
| W4 | **AOT compilation of untrusted wasm** | `wasmtime compile` produces `.cwasm` files that are only loaded by the trusted runtime. Do not run `wasmtime compile` on untrusted input. | Documented. |

### 2.4 VM & JIT (High)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| V1 | **JIT code injection** | JIT compilation is from MIR bytecode, not arbitrary machine code. `compile_region` stops at `Ret`. | Implemented. |
| V2 | **Type confusion in JIT** | `typed_compiler` strips NaN-tag guards only when `TypeMetadata` proves register types. Falls back to scalar compilation on ambiguity. | Implemented. |
| V3 | **Division-by-zero in JIT** | `IDiv`/`IMod` always emit runtime-helper calls (`nulang_idiv`/`nulang_imod`), never raw `sdiv`. | Implemented. |
| V4 | **SIMD out-of-bounds access** | Production SIMD is limited to proven Int64 kernel shapes. Before direct memory access, generated code validates each participating value is an Array and that lhs/rhs/destination lengths all cover the scalar trip count. Guard failure occurs before side effects and triggers a one-shot VM deopt to the original bounds-checked bytecode. | Implemented. |

### 2.5 Persistence & Storage (Medium)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| P1 | **SQLite injection in libsql persistence** | Values are serialized via Borsh, not string interpolation. | Implemented. |
| P2 | **CRDT tombstone accumulation** | `gc_tombstones` in `ORSet`/`AWORSet` removes entries below a configurable watermark. | Implemented. |
| P3 | **Data exfiltration via checkpoint serialization** | `heap_serialize` only serializes actor-local heap objects. No cross-actor data is included. | Implemented. |

### 2.6 Package Manager & Supply Chain (Medium)

| ID | Threat | Mitigation | Status |
|---|---|---|---|
| S1 | **Dependency confusion** | `nula` resolves from explicit git URLs or local paths. No public registry by default. | Implemented. |
| S2 | **Manifest tampering** | `Nulang.toml` is parsed with `toml` crate, not `eval`. No arbitrary code execution during parsing. | Implemented. |
| S3 | **Language version pin bypass** | `prepare_package` enforces `language` field against `LANGUAGE_VERSION_STR` at build/run/test time. | Implemented. |
| S4 | **Registry impersonation** | `nula publish` requires `NULANG_CLOUD_TOKEN` (Bearer auth). HTTPS is enforced by the registry URL. | Implemented. |

## 3. Attack Scenarios

### Scenario A: Malicious remote actor
An attacker gains access to a single node and sends a crafted `ActorMessage` to a remote actor.

- **Payload**: heap pointers, closures, and actor refs are rejected at send time (`packet_payload_wire_safe`). Only ints, floats, bools, unit, and strings cross the wire.
- **String injection**: strings travel by content (interned on receipt), so they cannot carry executable code unless the target behavior executes them.
- **Behavior name spoofing**: behavior names are resolved on the target node via `Runtime::behavior_id_for`. Unknown names are rejected; they never alias behavior id 0. If a sender includes a content hash for code fetch/hot-reload, the fetched artifact must still resolve the requested name before delivery.

### Scenario B: Compromised native library
An attacker replaces a `.so` file that a Nulang program loads via FFI.

- **Mitigation**: `--ffi-sandbox` restricts loading to an explicit allowlist. The operator must enumerate every library. Without the flag, `AllowAll` permits any library (default for development convenience).
- **Detection**: `cargo audit` in CI monitors `libloading` for CVEs. No runtime detection exists yet.

### Scenario C: Untrusted WASM module
An attacker uploads a `.wasm` module to a Nulang service that runs user code.

- **Mitigation**: Wasmtime's sandbox isolates linear memory. Host imports are limited to `IO.print`/`read` and `Array.*` operations. User-defined effects and closures are rejected at compile time.
- **Residual risk**: AOT compilation (`wasmtime compile`) must only be run on trusted input. The `.cwasm` format is not sandboxed.

## 4. Security Checklist for Operators

- [ ] Enable `TlsConfig::MutualTls` for all production clusters (not default yet).
- [ ] Configure `StaticQuorumResolver` with `expected_nodes > 1` to prevent split-brain.
- [ ] Use `--ffi-sandbox --ffi-allow /path/to/lib.so` in production. Never use `--ffi-sandbox` with an empty allowlist (it will deny all FFI).
- [ ] Monitor `Mailbox` depths and actor reduction counts for DoS.
- [ ] Run `cargo audit` weekly (automated in CI on every PR).
- [ ] Verify `.nbc` source hashes (`--verify <src>`) before running precompiled artifacts.
- [ ] Pin `language = "1.0.0-frozen"` in `Nulang.toml` to prevent accidental language-version drift.

## 5. Open Risks & Follow-up Work

| Risk | Priority | Tracking |
|---|---|---|
| WASM component capability gate (P5b) | High | Phase 5 — Security sandboxing |
| OTLP trace/metric export for security-event monitoring | Medium | Phase 3 — Observability |
| Windows CI matrix (no PyO3, no libsql) | Medium | Phase 4 — Windows support |
| Gossip amplification under Byzantine majority | Low | Research: need formal proof of convergence |
| JIT SIMD out-of-bounds on untyped arrays | Low | Typed compiler already guards this; untyped arrays do not support SIMD |

## 6. References

- `src/runtime/network.rs` — NUL0 wire protocol
- `src/runtime/cluster.rs` — `ClusterState`, `StaticQuorumResolver`
- `src/ffi/native.rs` — `FfiPolicy`, `FfiRegistry`
- `src/mir_wasm.rs` — WASM backend lowering
- `src/jit/typed_compiler.rs` — `TypeMetadata`, `infer_reg_types`
- `src/format/constants.rs` — `LANGUAGE_VERSION`, `LANGUAGE_VERSION_STR`
- `GOVERNANCE.md` — stability tiers and RFC process
- `SPEC2.md` — §"Security Considerations"
