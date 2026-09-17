# Semantic Stabilization Contract

Status: active stabilization policy for the pre-production language/runtime.

This document narrows Nulang development around one goal: make the semantics already claimed by the language deterministic, fail-closed, backend-consistent, and recoverable before expanding the public language surface.

It complements the stability tiers in `GOVERNANCE.md`, the consolidated architecture roadmap in issue #331, and the P0 stabilization gate in issue #231.

## Production thesis

Nulang's differentiating value is not feature count. It is the ability to preserve one program meaning across local execution, distribution, durability, migration, replay, and portable artifacts.

The target semantic chain is:

```text
source
  -> typed program
  -> inferred effects + authority requirements
  -> canonical semantic identity
  -> actor/protocol/state-schema identity
  -> artifact identity
  -> durable execution history
  -> runtime enforcement
```

A semantic claim is not production-ready until that chain preserves it end to end.

## Active P0 order

While this contract is active, correctness work takes precedence over new language/UI/AI surface area.

The dependency order is:

1. restore trustworthy exact-head executable CI;
2. close authority/ownership stabilization;
3. make actor behavior dispatch nominal and fail-closed;
4. preserve actor/protocol/schema identity through runtime activation and durable recovery;
5. enforce conservative match coverage diagnostics consistently across CLI/LSP/backends;
6. enforce migration purity and deterministic migration planning;
7. fence stale durable activations at the storage commit boundary;
8. make durable external effects receipt-backed and replay-safe;
9. bind durable history to canonical semantic/artifact identity and retain historical artifacts;
10. complete equivalent authority enforcement across bytecode/native/WASM host boundaries.

Issues #231 and #331 remain the authoritative dependency trackers when details differ.

## Merge rule during stabilization

The following work may merge when it directly advances or proves the P0 chain:

- correctness and security fixes;
- conformance and differential tests;
- exact-head CI and reproducible build work;
- deterministic semantic identity/provenance;
- durability, migration, replay, and authority hardening;
- compiler-visible contracts required by the already-existing universal-app vertical slice;
- observability needed to prove those invariants.

The following work should remain draft or experimental unless it directly unblocks a P0 invariant:

- new Frozen/Stable syntax;
- new ownership/capability models parallel to the current system;
- additional actor identity/protocol hierarchies;
- additional durable-effect models;
- new backends added primarily for feature parity;
- broad AI-specific core syntax;
- new UI primitives beyond the existing experimental web/mobile vertical slice;
- generic remote/distributed effects that duplicate actor semantics.

## One semantic source of truth

Nulang must have one compiler-owned interpretation of language semantics.

Compiler-derived facts such as these must flow forward rather than be reconstructed independently by consumers:

- canonical types;
- inferred effect rows;
- capability/authority requirements;
- actor behavior/protocol identity;
- state-schema and migration identity;
- semantic IDs and artifact IDs;
- web/request contract metadata;
- deployment-relevant behavior metadata.

Backends and Nulang Cloud may lower, transport, enforce, schedule, or persist these facts. They must not invent a second source-language interpretation.

## Backend contract

Until the stabilization gate closes:

- the bytecode VM is the semantic reference implementation;
- WASM is the canonical portable/cloud execution target;
- JIT is an optimization layer;
- native AOT remains secondary where full semantic parity is not yet demonstrated.

A backend optimization is acceptable only when differential/conformance evidence shows it preserves the checked semantics.

## Durable execution contract

A durable activation must never silently reinterpret history under unrelated code.

The target persisted identity set is conceptually:

```text
semantic_id
artifact_id
state_schema_id
state_schema_version
protocol_id (when applicable)
runtime_semantics_version
activation_epoch
```

Recovery must follow exactly one path:

1. prove direct compatibility and resume;
2. execute a complete deterministic migration chain and atomically commit the upgraded identity/state;
3. fail closed without mutating the last valid state.

Missing historical code, ambiguous identity, corrupt history, stale activation epochs, incompatible schemas, and unknown protocol behavior are errors, never implicit coercions.

## External-effect contract

Durability does not imply exactly-once execution against arbitrary external systems.

For durable external effects, Nulang should provide:

- stable logical invocation identity;
- persisted intent before execution;
- provider idempotency keys where supported;
- persisted terminal receipts/results;
- replay that returns a committed receipt rather than re-executing;
- explicit handling of indeterminate crash windows;
- activation fencing so stale writers cannot commit receipts.

Claims stronger than effectively-once observation require participation from the external provider or a stronger transaction protocol.

## Web/mobile scope during stabilization

The existing hybrid rendering decision remains in force:

- Web lowers to the DOM.
- Mobile lowers to native platform controls.
- Desktop uses the system WebView.
- Behavioral parity is the cross-platform goal; pixel identity is not.

The Web Contract IR is the preferred pattern for framework evolution: compiler-visible contracts should feed runtime dispatch, deployment metadata, OpenAPI/client generation, tests, and Cloud without introducing a second source parser.

Fine-grained reactivity, resource/action abstractions, streaming SSR, islands, server functions, and richer cross-platform component semantics remain good post-gate experiments. They should not outrank semantic correctness.

## Verification gate

A candidate production baseline requires exact-head executable evidence, not merely source review or workflow badges.

At minimum:

```text
cargo fmt --all -- --check
cargo check --all-targets
cargo check --all-targets --no-default-features
warning/Clippy gates
default + minimal tests
relevant conformance tests
relevant differential tests
git diff --check
```

Hardware-specific/KVM lanes may remain separate. Ordinary Rust correctness must not depend on mutable hardware runners.

A workflow run with zero created jobs is not validation evidence.

## Exit test

The stabilization contract can be relaxed only after a representative durable program can repeatedly survive fault injection while preserving semantic identity and authority.

The minimum end-to-end proof is:

```text
compile
  -> emit canonical artifact + semantic metadata
  -> execute durable actor
  -> perform a replay-safe external effect
  -> commit durable state
  -> kill runtime/process at controlled boundaries
  -> recover under the same or explicitly migrated semantic identity
  -> prove no stale writer committed
  -> prove no completed logical effect was duplicated
  -> prove state/protocol/authority remained correct
```

Run this through deterministic simulation and destructive process-kill tests across supported persistence/runtime paths.

## Frozen-core review

The current repository describes the language as alpha and pre-external-user while also carrying a `1.0.0-frozen` compatibility contract. That tension should be reviewed explicitly rather than resolved ad hoc through implementation patches.

Until a dedicated RFC decides otherwise:

- do not casually break Frozen Core;
- do not expand Frozen Core;
- document mismatches truthfully;
- prefer warnings/strict modes when a correctness improvement conflicts with an existing frozen validity rule;
- collect concrete evidence for any future compatibility-policy revision.

The goal is to preserve user trust without letting premature compatibility promises force silent semantic defects.