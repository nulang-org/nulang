# RFC 0003: Content Identity and Addressed Code

- **Status:** Draft (revised 2026-09-22)
- **Author:** Nulang Core Team
- **Created:** 2026-07-29
- **Stability Tier:** Experimental
- **Related:** RFC 0001, RFC 0019, RFC 0020

## Summary

Nulang uses content identity for several different purposes, but those purposes
have different invalidation rules. A single hash of source text, type signatures,
and backend bytecode is therefore not a sufficient long-term identity model.

This RFC defines four distinct identities and one effect-site locator:

```text
SourceId
  identity of exact source/package inputs

SemanticId
  identity of canonical typed/lowered program semantics

ArtifactId
  identity of a semantic program compiled with a particular
  compiler/target/ABI/backend/codegen configuration

ArtifactDigest
  digest of the exact emitted executable bytes

EffectSiteId
  stable locator for an explicit effect occurrence inside one semantic owner
```

`SourceId`, `SemanticId`, and `ArtifactId` are already represented in
`src/content_identity.rs`. Canonical MIR-derived semantic identity lives in
`src/semantic_identity.rs`, and artifact provenance is represented by
`ArtifactIdentityManifest` in `src/artifact_identity.rs`.

`ArtifactDigest` is deliberately separate from `ArtifactId`: deployment and
artifact fetching must bind to the exact executable bytes, while durable
compatibility should reason about program semantics rather than a particular
code-generation result.

## Why the original design changed

The original draft proposed one function `content_hash` derived from source
identity, a type signature, and compiled bytecode. That conflated three
questions:

1. Did the source/package inputs change?
2. Did the executable semantics change?
3. Did the emitted machine/Wasm/bytecode artifact change?

Those questions must not share one identity.

A formatting-only edit can legitimately change source identity while preserving
semantics. The same semantics can be emitted as bytecode, native code, or Wasm
and therefore produce different artifacts. A compiler upgrade can produce
different bytes without changing the durable program contract.

Long-lived actors and workflows need those distinctions to survive compiler,
backend, and infrastructure evolution safely.

## Identity model

### SourceId

`SourceId` answers:

> Are these the same exact source/package inputs?

Conceptually:

```text
SourceId = BLAKE3(domain || canonical package-input framing)
```

Source identity is useful for provenance, source caches, reproducible-build
diagnostics, and lockfile/package inputs. Presentation-only source changes may
change it.

### SemanticId

`SemanticId` answers:

> Does this definition/program have the same compiler-owned semantics?

Conceptually:

```text
SemanticId = BLAKE3(
  domain
  || canonical typed/lowered semantics
  || referenced SemanticIds
)
```

The canonical representation must exclude presentation and backend details such
as source spans, debugger line tables, raw compiler allocation IDs, declaration
vector positions, machine target, and backend selection.

It must include semantics that can affect observable behavior or durable
compatibility, including typed state schemas, effect requirements, authority
requirements, actor/workflow behavior, and other compiler-owned semantic data.

`src/semantic_identity.rs` is the compiler-owned implementation boundary.

### ArtifactId

`ArtifactId` answers:

> Is this the same semantic program under the same code-generation recipe?

Conceptually:

```text
ArtifactId = BLAKE3(
  domain
  || SemanticId
  || compiler identity/version
  || target
  || platform ABI
  || backend
  || codegen-relevant flags
)
```

`ArtifactId` is therefore a build/configuration identity. It intentionally
changes when target or backend changes, even when `SemanticId` is unchanged.

It is not by itself proof that two stored byte blobs are identical.

### ArtifactDigest

`ArtifactDigest` answers:

> Are these the exact executable bytes that were admitted or fetched?

Conceptually:

```text
ArtifactDigest = BLAKE3(executable_bytes)
```

Deployment systems must verify this digest against the bytes they execute.
RFC 0020's Behavior Manifest binds deployment semantics to the exact executable
artifact through such a digest.

This distinction is security-relevant:

```text
SemanticId    -> compatibility / durable meaning
ArtifactId    -> build-cache / codegen identity
ArtifactDigest -> exact-byte integrity / admission / fetch
```

### EffectSiteId

`EffectSiteId` identifies one explicit effect occurrence inside compiler-owned
lowered semantics. It is a locator, not a replacement for `SemanticId`.

A durable effect invocation should ultimately derive identity from:

```text
owner SemanticId
  + EffectSiteId
  + durable activation/turn identity
  + occurrence index
  -> DurableEffectId
```

This avoids unstable identities derived from source locations, bytecode PCs,
runtime actor addresses, or incidental declaration ordering.

## Durable compatibility

Durable actors, entities, and workflows must not blindly pin to exact machine
code when their durable state model can be resumed by semantically compatible
code.

The default decision boundary should be:

```text
same SemanticId
  -> exact semantic identity

different SemanticId
  -> require an explicit compatibility/migration decision
```

Exact `ArtifactDigest` remains necessary when restoring backend-specific state
whose representation genuinely depends on executable bytes, such as an opaque
engine snapshot that cannot be reconstructed from semantic state alone.

Nulang should prefer durable state/checkpoint formats that minimize this
backend coupling.

## Addressed code distribution

Code mobility and registries should fetch executable artifacts by exact
`ArtifactDigest`, never merely by `SemanticId`.

Conceptual flow:

```text
actor/workflow references semantic identity
        |
        v
deployment/runtime selects an admitted target artifact
        |
        v
ArtifactId + ArtifactDigest
        |
        v
fetch exact bytes by ArtifactDigest
        |
        v
verify digest + manifest + ABI compatibility
        |
        v
execute
```

A `SemanticId` may map to multiple valid artifacts:

```text
SemanticId S
  |- x86_64 native ArtifactId A1 -> digest D1
  |- aarch64 native ArtifactId A2 -> digest D2
  `- wasm component ArtifactId A3 -> digest D3
```

This is expected, not an identity failure.

## Registry requirements

An addressed-code registry should eventually support:

- exact artifact lookup by `ArtifactDigest`;
- metadata lookup by `ArtifactId`;
- search/indexing by `SemanticId` and target profile;
- immutable artifact bytes;
- Behavior Manifest and artifact-identity metadata stored alongside the blob;
- compiler/provenance metadata;
- optional publisher signatures/attestations;
- garbage collection based on durable references and retention policy.

Content hashing proves integrity, not authorship. Signatures or attestations are
a separate provenance layer.

## Package management

Dependency/package resolution should not put backend-specific `ArtifactId`
semantics into `Nulang.lock` as though they were source dependency identity.

Package metadata may record `SourceId` and `SemanticId` for resolved inputs.
Compiled artifacts belong in build metadata/registries and can be regenerated
for another target without mutating the dependency graph.

## Frozen `.nbc` v1 compatibility

RFC 0001 freezes the published `.nbc` v1 format. This RFC does not add fields
to that header in place.

Identity metadata must remain additive through sidecar/versioned manifests or a
future explicitly versioned `.nbc` format revision.

Existing `source_hash` fields retain their historical meaning and must not be
silently reinterpreted as `SemanticId`, `ArtifactId`, or `ArtifactDigest`.

## Behavior Manifest integration

RFC 0020 is the deployment-facing semantic envelope. It should carry or bind
the identities needed by admission without exposing HIR/MIR implementation
details.

Conceptually:

```text
Behavior Manifest
  source/provenance identity
  SemanticId
  ArtifactId/build provenance
  ArtifactDigest
  protocol IDs
  state schema IDs
  effects/replay classes
  required authority
  host ABI/WIT versions
```

Cloud/runtime policy may restrict what the manifest requests. The manifest
cannot grant authority by itself.

## Migration from the original RFC 0003 model

1. Keep historical source hashes readable with their original meaning.
2. Use `SourceId`, `SemanticId`, and `ArtifactId` as separate compiler/runtime
   types.
3. Derive `SemanticId` from canonical backend-independent typed/lowered
   semantics.
4. Bind deployment to the exact executable bytes with `ArtifactDigest`.
5. Introduce `EffectSiteId` for compiler-owned effect locations.
6. Move durable effect invocation identity away from runtime actor IDs/source
   positions toward semantic owner + site + durable execution identity.
7. Add registry/code-mobility resolution only after the identity and
   compatibility contracts are executable and tested.

## Required invariants

Before content-addressed distributed execution is considered stable:

1. Formatting/source-location-only changes can change `SourceId` without
   changing `SemanticId`.
2. Supported backends for the same semantics share `SemanticId`.
3. Backend/target/compiler configuration changes produce the appropriate
   distinct `ArtifactId`.
4. Exact executable-byte changes are detectable through `ArtifactDigest`.
5. Deployment rejects a manifest whose artifact digest does not match the
   bytes being executed.
6. Durable state compatibility uses semantic/state-schema rules rather than
   accidental bytecode identity.
7. Effect invocation identity is replay-stable across process/node addresses
   and source formatting changes.
8. Unknown identity/schema versions fail closed.

## Non-goals

This RFC does not:

- define semantic equivalence between arbitrary independently written programs;
- make different `SemanticId`s automatically compatible;
- claim exact-once external side effects;
- require distributed artifact storage to be peer-to-peer;
- require IPFS or another external content-addressing protocol;
- change frozen `.nbc` v1 bytes;
- make source formatting irrelevant to provenance.

## Implementation status

As of 2026-09-22:

- `SourceId`, `SemanticId`, and `ArtifactId` are implemented as domain-separated
  BLAKE3 identities in `src/content_identity.rs`;
- canonical backend-independent MIR semantic identity is implemented in
  `src/semantic_identity.rs`;
- versioned artifact identity/provenance metadata exists in
  `src/artifact_identity.rs`;
- RFC 0020 defines the Behavior Manifest deployment contract;
- compiler-owned effect-site identity is being integrated separately;
- a general addressed-code registry / on-demand code-fetch protocol remains
  experimental future work.

## References

- RFC 0001 — Format Stability
- RFC 0019 — Semantic Closure Before Further Surface Expansion
- RFC 0020 — Nulang Behavior Manifest
- Unison — content-addressed code model
- BLAKE3
