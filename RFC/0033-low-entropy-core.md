# RFC 0033: Low-Entropy Core and Dependency Boundaries

- **Status:** Draft
- **Tier:** Experimental (implementation/build policy, no source-language change)
- **Created:** 2026-10-27
- **Supersedes:** none
- **Superseded by:** none

## Summary

Preserve a small, independently verifiable semantic core and bytecode reference VM while making advanced Nulang capabilities composable rather than required for basic programs. Initially enforce a resolved minimal-profile dependency boundary in CI. **Do not change default features, public release profiles, language semantics, or published artifacts in this RFC's first implementation.**

Low entropy means few mutually dependent semantics, reliable interfaces, reproducible builds, restrained external packages, and explicit long-term compatibility obligations. It does not mean rewriting security-sensitive dependencies in-house or deleting durable/actor/distributed functionality.

## Motivation

Nulang currently combines a typed core with actor isolation, effects, capabilities, durable execution, multiple compiler backends, native/Python interop, AI, web, Fabric, cache, and several storage adapters. The base Cargo package defaults to the historical full profile, and public release CLI has its own feature set. Each cross-layer dependency increases upgrade, security, release, and conformance costs, even when a small Nulang program does not use those features.

The existing CI job called minimal-build already compiles, tests, and checks with --no-default-features. That demonstrates the build works but does not explicitly reject *new transitive optional integrations* added to the minimal closure.

This RFC complements RFC 0024 (orthogonal execution domains) and docs/SEMANTIC_STABILIZATION_CONTRACT.md. Its implementation must not outrank exact-head CI, canonical semantics, authority correctness, or durable crash/recovery hardening.

## Design

### Dependency ownership matrix

| Component | Owns | Must not require |
| --- | --- | --- |
| Semantic compiler and bytecode VM | types, effect/capability analysis, IR, bytecode, diagnostics | AI providers, database adapters, PyO3, Cranelift, Wasmtime or Cloud |
| Local actor runtime | isolated heaps, scheduling, mailboxes, supervision, host interfaces | AI/web products, cloud control planes, remote services for local execution |
| Durability and distribution | persistence, replay, transport, membership with explicit versioned contracts | independent interpretations of source semantics |
| Optional JIT/native/WASM backends | optimizations/lowering from compiler-owned IR | reimplementation of source-language semantics |
| Application adapters | databases, web, AI, Fabric, cache, integrations via stable interfaces | reverse dependency from semantic core to adapter implementations |

This matrix records **desired ownership**, not proof that current Rust modules satisfy every boundary. The first executable enforcement concerns the root crate's activated Cargo dependency graph. Source-module extraction is deferred to separate, evidence-bearing work.

### Immediate executable gate

scripts/check_core_dependencies.py runs the locked Cargo metadata resolver with the nulang root package and --no-default-features. It traverses only activated *normal and build* edges reachable from that package, excluding dev-only edges and unrelated workspace packages. Cargo metadata may mention dependencies of other workspace packages, so checking all listed packages would be incorrect.

The check fails closed on missing/incomplete graph data and rejects these optional integration families in the minimal runtime dependency closure:

- compiler optimizers and portable host tooling: Cranelift family, Wasmtime, wasm-encoder, wat, borsh;
- AI: nulang-ai family;
- Python: pyo3;
- data systems: libsql, rocksdb, postgres;
- asynchronous/network/UI integrations: tokio, tower-lsp, reqwest, ureq, mio;
- native dynamic FFI: libffi, libloading;
- telemetry integrations: OpenTelemetry family and tracing-opentelemetry.

This is an initial *categorical budget*: zero forbidden optional integration families in the no-default-features runtime dependency closure. The check is deliberately not a ban on serde, logging, TLS, cryptography, or all third-party code. The guard does not itself prove source-module isolation or guarantee a zero-external-dependency binary.

The existing minimal-build CI job will execute the guard before compiling. Python unit tests use synthetic resolved metadata to prove transitive detection, expected normal/build edges, dev-only/workspace exclusion, and fail-closed behavior. Actual Cargo checks and live package graphs must still run in exact-head CI.

### Complexity and dependency budgets

Each new required dependency or semantic primitive should document:

1. Whether a typed function, effect, actor, scoped task, or separate library could solve the same problem.
2. New build/runtime services, platform constraints, transitive packages, and security maintenance obligations.
3. Added backend, artifact, CI, migration, or conformance obligations.
4. An explicit removal or isolation plan if the dependency is experimental.

Record per-profile **measured** package closure, binary size, reproducible build time, startup, and known runtime services on supported CI hosts. Set numerical ceilings only after recording reproducible baselines. A raw crate-count ceiling alone is a poor proxy for security/maintenance risk: a vetted crypto upgrade may legitimately increase package count.

The first blocking budget in this PR is the no-default-features categorical boundary, not an invented dependency-count threshold.

### Compatibility and rollout constraints

- Keep Cargo default full, release-cli, and existing binaries unchanged in Phase 1.
- Do not reinterpret .nbc, NUL0, value layout, actor identity, persisted history, or built-in frozen effects.
- Do not add new stable syntax or alter type/effect/authority semantics through this RFC.
- Maintain the bytecode VM as the semantic reference; any backend transformation requires differential/conformance evidence.
- No change may merge on a green badge with no executable jobs, skipped checks, or evidence from a different head commit.
- A discovered *existing* boundary violation should be investigated and fixed, not silently added to an allowlist. If genuinely essential, the architecture policy itself needs review and a documented exception.

### Deferred work

1. Gather live, same-runner dependency-closure and build/binary metrics for full, release-cli, and minimal profiles.
2. Make core runtime/adapter interfaces explicit; extract crates only when reversing an actual dependency and without changing behavior.
3. Keep AI agents, web, Fabric/cache, persistence, and Cloud as optional capabilities; avoid competing semantic interpreters.
4. Review developer experience (one formatter, one CLI, sensible defaults), but preserve compatibility through separate changes.
5. Reconsider a smaller default installation only after stabilization, full/release compatibility validation, and external adoption evidence.

## Tier Classification

Experimental architecture/build policy; no alteration to published Frozen formats or Stable language semantics. RFC remains Draft pending steward review. The initial non-breaking guard can be trialed independently.

## Backwards Compatibility

Source and package interfaces are unchanged. Historical full and release-cli profiles are preserved. The only new fail condition is on CI when optional backend/integration crates enter the no-default-features resolved runtime closure. Intentionally adding such a dependency requires a separate documented decision.

## Alternatives Considered

- Immediately default to core: rejected as potentially breaking prebuilt users and current release validation.
- Delete advanced capabilities: rejected because they can reduce total application-level complexity.
- Rewrite storage/TLS/crypto to eliminate external crates: rejected due security and maintenance risk.
- Grep Cargo.toml alone: rejected because manifest text cannot detect transitive activation.
- Ban all third-party packages: rejected as an inaccurate proxy for maintenance entropy.

## Open Questions

- What profile metrics are reproducible across supported release targets?
- Which host effect and durable storage interfaces can become stable without duplicating compiler-owned semantics?
- Which full-profile compatibility obligations must be retained before changing default installation packaging?

## Resolution

Pending review and exact-head executable CI results.
