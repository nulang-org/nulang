# RFC 0032: Incubating Core and Representation-Semantics Separation

- **Status:** Proposed for merge (maintainer-directed pre-adoption change)
- **Tier:** Governance
- **Created:** 2026-10-17
- **Supersedes:** RFC 0021 section 2 (pre-adoption Core must be Stable) and its source-semantic RFC/deprecation requirement; does not supersede its published-format compatibility rules
- **Language version at effect:** policy-only; existing format/language version 1 metadata remains unchanged

## Decision

Nulang has no independently deployed production applications. The Core should be
**Incubating** rather than Frozen or Stable until external adoption proves it
deserves long-term semantic constraints. Correctness, simplicity, and
cross-backend semantics outrank premature source compatibility.

An Incubating change needs a clear behavioral contract, targeted regression
and differential tests where appropriate, and updated specification/changelog.
It does **not** need a new RFC or a multi-major deprecation period. Separately
deployed or persisted representations still require explicit versions,
migration/readers, or deterministic incompatibility diagnostics.

This decision does not assert that previously published binaries, durable
states, or wire messages do not exist. Keep v1 readers and safety checks until
the format inventory proves they can be retired. A label such as
`1.0.0-frozen` in historical artifacts does not freeze the current language.

## Representation invariants

- Nulang `Value` is presently one 64-bit word, with 48-bit immediate payloads.
- Host pointer width, VM handle width, durable identity width, and the width
  of the language `Int` are **different concerns**.
- Runtime handles must never silently mask away high ID bits.
- Virtual-actor hashes must never overwrite another known identity locally.
  This is fail-closed collision detection, not globally unique identity.
- Float NaNs must be canonicalized when entering a `Value` to prevent type
  tag aliasing.
- Native/bytecode/WASM/JIT/AOT must agree on arithmetic and serialization.

## Staged implementation

### Phase 1: Fail closed without changing v1 encoding

1. Make actor-reference construction reject IDs beyond the inline payload.
2. Reject known grain-hash collisions before changing actor/persistence indices or delivering cross-shard traffic to an unrelated resident actor.
3. Canonicalize floats through the existing `float_bits` function.
4. Pin the behavior with regression tests.

These are safety fixes. They do not claim full 64-bit actor identities or
64-bit `Int` semantics.

### Phase 2: Explicit numeric semantics

Design the language-level `Int` as signed 64-bit (or decide deliberately on
arbitrary precision), with one documented overflow rule. Keep inline 48-bit
small integers. Represent out-of-range immediate integers with an appropriate
owned wider representation where dynamically boxed values are required,
while retaining native `i64` in typed SSA and arrays. Audit constants,
FFI, persistence, snapshot serialization, migrations, and JIT/AOT/WASM
arithmetic for equivalent semantics.

The current mixed behavior (masking, wrapping, and checked negation) is **not**
a suitable final numeric contract. No backend should silently truncate a
language-level `Int` once the new contract is implemented.

### Phase 3: Split logical actor identities from local handles

Use collision-checked, process-local compact handles. Durable actor identities
must retain their full logical (type, key, namespace) representation; globally
routed messages must resolve that identity unambiguously. Do not serialize a
48-bit grain hash as the sole authority for a durable identity. Version the
wire/state envelope when this boundary changes.

### Phase 4: Consolidate memory references

Remove legacy raw-host-pointer-tagging in favor of validated heap-relative
offsets or owned generation-aware handles. Validate FFI and object lifetime
requirements before claiming portability to high virtual addresses.

## Acceptance gates

- Local: `cargo fmt --all -- --check`, focused new unit tests,
  `cargo test --no-default-features --lib`, and integration tests.
- Repository: exact-head `cargo check --all-targets`, default/minimal CI,
  differential VM/JIT/AOT/WASM tests, conformance, and security lint.
- Semantic transitions: migration/version diagnostics when bytecode, wire,
  persistence, or embedder ABI changes.
- Do not merge on the basis of an unrelated docs deployment or empty CI job.

## Scope of this PR

Only Phase 1 and governance changes are implemented here. Phases 2–4 are
explicitly deferred because a partial implementation would create more
semantic inconsistencies than the current 48-bit contract.
