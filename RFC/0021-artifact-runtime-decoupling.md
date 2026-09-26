# RFC 0021: Decouple Durable Artifacts and Wire Values from Runtime Representation

- **Status:** Draft
- **Tier:** N/A (architecture/format evolution; no Core syntax change)
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-09-20
- **Resolved:** TBD
- **Language-version at effect:** N/A
- **Supersedes:** none
- **Superseded by:** none

## Summary

Preserve RFC 0001's strongest guarantee — every published `.nbc` and NUL0
format version is immutable and old artifacts remain readable through explicit
versioned migration — while removing an unnecessary coupling between those
external contracts and Nulang's current VM internals.

The design establishes three distinct layers:

1. **External durable/wire representation** — versioned, canonical, immutable
   once published.
2. **Semantic artifact representation** — portable program/value information
   required to reconstruct Nulang semantics.
3. **Runtime representation** — bytecode opcode layout, register encoding,
   NaN/tagged `Value` representation, JIT ABI, scheduler structures, GC
   metadata, and other implementation details.

A published format version remains frozen. The current runtime representation
does not.

This RFC does **not** invalidate `.nbc` v1, NUL0 v1, or RFC 0001. It defines
how v2+ formats should evolve so future VM optimization does not require
preserving today's internal instruction and value layouts indefinitely.

## Motivation

RFC 0001 correctly solved a real compatibility problem: prior to versioned
artifacts and wire framing, changing an opcode or packet shape could silently
invalidate stored programs or peers. The resulting magic/version headers,
unknown-version rejection, and migration registry are the right foundations.

The current v1 implementation, however, serializes `Instruction::encode()`
directly and describes `VALUE_LAYOUT_VERSION` as pinning the runtime's tagged
value representation for both compiled artifacts and wire peers. That makes
three separate concerns evolve together:

```text
persistent artifact representation
          ↕
internal VM instruction/value representation
          ↕
network representation
```

This is stronger coupling than Nulang semantics require.

It creates a long-term optimization tax. Changes such as:

- widening registers or instruction operands;
- fusing or splitting internal opcodes;
- changing register allocation;
- replacing the current tagged/NaN-boxed value layout;
- changing GC metadata;
- specializing interpreter/JIT-only instructions;
- introducing a different interpreter dispatch format;
- changing pointer payload widths for new architectures;

should not require changing the source language, Nulang Core, or network
semantics merely because the current implementation serialized those internal
details directly.

Long-lived systems normally freeze **external representation**, not every
internal representation used to execute it.

## Goals

1. Keep every published `.nbc` version byte-for-byte immutable.
2. Keep old `.nbc` artifacts runnable through explicit decode/migration.
3. Keep NUL0 framing/version compatibility explicit and fail-closed.
4. Permit internal bytecode/value/JIT/GC representation to evolve independently.
5. Make backend equivalence depend on semantics, not identical internal layouts.
6. Use one canonical wire encoding independent of host CPU representation.
7. Avoid negotiation state that does not provide semantic value.

## Non-Goals

- Changing Nulang syntax or Core semantics.
- Replacing RFC 0001's migration registry.
- Rewriting `.nbc` v1 in place.
- Introducing an immediate `.nbc` v2.
- Replacing NUL0 v1 immediately.
- Standardizing MIR as a public interchange format.
- Promising that every compiler implementation must share one internal IR.

## Design

### 1. Published format versions remain frozen

RFC 0001 remains authoritative:

> A version, once published, is immutable.

Therefore:

```text
.nbc v1 bytes
    ↓
v1 decoder
    ↓
semantic/current module
```

A future implementation may no longer use v1's instruction encoding
internally, but it must continue to decode v1 correctly.

If a new durable representation is needed:

```text
.nbc v1 ──decode/migrate──▶ current representation
.nbc v2 ──decode──────────▶ current representation
```

There is no "reinterpret v1 using the new layout" path.

### 2. Separate serialized instructions from live VM instructions

Today v1 encodes each instruction through `Instruction::encode()`. That is
frozen **for the v1 codec**, not as a permanent requirement on the live VM.

For v2+, format code SHOULD own its serialized instruction schema explicitly:

```text
format/v1 instruction codec
format/v2 instruction codec
             │
             ▼
      decoded semantic code
             │
             ▼
     current VM lowering
```

The current VM may use a different instruction type, width, opcode numbering,
or fused instructions after decoding.

A format codec must not depend on "whatever `Instruction` happens to look
like in this compiler build."

### 3. Runtime `Value` layout is not a wire format

The logical Nulang value model is semantic. The physical runtime layout is an
optimization.

Future NUL0 and durable-value codecs MUST serialize logical values through a
canonical value codec rather than copying or interpreting runtime tagged words.

For example:

```text
runtime Value
   ↓ encode logical value
canonical wire/durable value
   ↓ decode
runtime Value
```

This allows:

```text
48-bit payload + 16-bit tag
        ↓ future runtime
different tags / wider payload / boxed representation
```

without changing NUL0 semantics.

`VALUE_LAYOUT_VERSION` may continue to identify VM-local or artifact-local
representation where required, but a future runtime-layout bump must not
automatically imply a wire-protocol bump.

### 4. Fixed canonical byte order; no endianness negotiation

NUL0 should use a single canonical byte order for every multi-byte wire field.

Peers do not negotiate CPU endianness.

```text
host representation
      ↓ encode
canonical NUL0 bytes
      ↓ decode
peer representation
```

This remains true on little-endian, big-endian, 32-bit, 64-bit, 128-bit, or
future architectures.

If NUL0 v2 is introduced, it MUST document one fixed byte order. Endianness
negotiation proposed as a future possibility in RFC 0010 is superseded by this
rule.

Rationale: negotiation expands protocol state space without adding a semantic
capability. Byte-order conversion belongs at the transport boundary.

### 5. Stability boundaries

The following are **external/versioned** contracts:

- Nulang source-language semantics;
- Frozen Core semantics;
- published `.nbc` format versions;
- published NUL0 protocol versions;
- package/manifest schema versions;
- Behavior Manifest versions where applicable;
- durable state/event envelope versions;
- externally documented Component/WIT contracts.

The following are **internal by default** and MAY evolve without a language
major-version bump, provided external semantics remain unchanged:

- live interpreter opcode enum/numbering;
- register width and allocation strategy;
- VM frame layout;
- runtime `Value` tagging/boxing;
- GC metadata layout;
- scheduler queue structures;
- Cranelift helper ABI;
- JIT region representation;
- MIR implementation details not explicitly standardized;
- cache/layout specializations internal to the runtime.

If an internal structure is embedded directly into a published external
format, the **codec for that published version** becomes frozen — not the
live structure itself.

### 6. Loader boundary

The loader becomes the compatibility boundary:

```text
artifact bytes
  ↓
magic/version validation
  ↓
version-specific decoder
  ↓
migration if required
  ↓
semantic module
  ↓
current backend lowering
```

The version-specific decoder must reject malformed or unsupported data before
creating live VM state.

A future `.nbc` v2 should prefer semantic identifiers/tables over exposing
implementation-only opcode numbering where practical.

### 7. Backend equivalence

Interpreter, JIT, and WASM backends are required to agree on observable Nulang
semantics, not on their internal representation.

The reference interpreter remains the semantic oracle for differential tests.

Recommended target:

```text
                  MIR
              /         \
     current bytecode   WASM
        /       \         \
 interpreter   JIT       Cloud
```

An implementation may introduce internal fused/specialized instructions after
MIR without changing `.nbc` format semantics.

### 8. Migration registry responsibilities

`src/format/migrate.rs` remains the sole legal home for durable artifact
format upgrades.

Migrations may:

- transform vN serialized schema to vN+1;
- decode an old format into an intermediate semantic representation and
  re-encode it;
- reject transformations that cannot preserve semantics.

Migrations must not depend on accidental current VM memory layout.

### 9. Compatibility tests

CI should eventually contain golden fixtures for every published format
version.

For `.nbc`:

1. load historical fixture bytes;
2. validate exact version-specific decoding;
3. execute through the current runtime;
4. compare observable behavior with the fixture's expected result.

For NUL0:

1. decode historical packet fixtures;
2. reject unknown versions;
3. verify canonical byte order;
4. round-trip logical values without assuming current `Value` word layout.

These fixtures are more meaningful compatibility guarantees than requiring the
current VM's opcode enum to remain unchanged forever.

## Migration Plan

### Phase A — documentation only

- Accept this RFC.
- Clarify RFC 0001/SPEC2 language so "v1 is frozen" cannot be read as "the live
  VM instruction/value implementation is frozen forever."
- Document fixed canonical wire byte order.

No binary bytes change.

### Phase B — codec isolation

- Move v1 serialized-instruction decoding behind an explicit v1 codec type.
- Move durable/wire value serialization behind a logical value codec.
- Add golden v1 artifact and NUL0 fixtures.

No v1 bytes change.

### Phase C — representation freedom

Only after differential/golden tests exist:

- allow internal opcode changes without modifying the v1 codec;
- allow runtime `Value` representation experiments behind the logical codec;
- keep v1 decode compatibility.

### Phase D — v2 only when justified

Introduce `.nbc` v2 or NUL0 v2 only when an external format change provides a
measurable capability, size, performance, or portability benefit.

Internal refactoring alone is not sufficient reason for a new external format
version.

## Alternatives Considered

### Freeze the live VM representation forever

Rejected. It maximizes implementation compatibility at the cost of permanently
constraining interpreter, JIT, memory, and architecture evolution.

### Make `.nbc` entirely unstable before 1.0

Rejected. RFC 0001 already provides useful long-lived artifact guarantees, and
there is no need to discard them to regain implementation freedom.

### Persist MIR directly

Deferred. MIR is currently an internal compiler structure and may itself
evolve. This RFC defines the boundary without requiring MIR to become public.

### Negotiate endianness in NUL0

Rejected. Canonical byte order is simpler, deterministic, easier to test, and
independent of host architecture.

### Serialize raw runtime `Value` words for speed

Rejected for external formats. It couples network/durable compatibility to
pointer/tag layout and creates portability/security hazards. Fast internal
same-process paths may continue using runtime-native representations.

## Interaction with Existing RFCs

- **RFC 0001 — Format Stability:** preserved. Published format versions remain
  immutable; this RFC clarifies that their codecs, not all future VM internals,
  carry that obligation.
- **RFC 0002 — Frozen Core:** unchanged.
- **RFC 0010 — 100-Year Language Architecture:** reinforces its principle of
  stable semantics and replaceable backends, but replaces the suggested future
  NUL0 endianness negotiation with canonical fixed byte order.
- **RFC 0019 — Semantic Closure:** complementary; backend parity is semantic.
- **RFC 0020 — Behavior Manifest:** complementary; externally relevant behavior
  remains portable and inspectable independently of internal representation.

## Acceptance Criteria

This RFC is complete when the project agrees that:

1. `.nbc` v1 and NUL0 v1 remain frozen and supported as documented.
2. A published format codec may be frozen without freezing the live VM data
   structure it currently resembles.
3. Runtime `Value` layout is not inherently a network representation.
4. Future NUL0 uses one canonical byte order, not endianness negotiation.
5. Backend/internal representation changes are permitted when differential and
   compatibility tests prove observable semantics remain unchanged.

## Open Questions

1. Should the first isolated v1 instruction codec be a dedicated
   `format::v1::InstructionV1` type or a compact codec module around the
   existing enum?
2. Should canonical logical-value encoding live under `format/`, a shared
   `protocol/` module, or the future Component ABI package?
3. When v2 is eventually justified, should it store semantic/basic-block
   instructions or continue storing a compact execution-oriented bytecode?
