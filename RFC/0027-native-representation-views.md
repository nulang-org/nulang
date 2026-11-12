# RFC 0027: Native Representation Views

- **Status:** Draft
- **Tier:** Experimental
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-09-17
- **Updated:** 2026-09-27
- **Resolved:** TBD
- **Language-version at effect:** N/A until accepted
- **Supersedes:** none
- **Superseded by:** none
- **Related:** RFC 0001 (format stability), RFC 0021 (compatibility before freeze)

## Summary

Introduce explicit, opt-in physical representation metadata for Nulang record
types without changing their logical type semantics or the frozen VM value
layout. The first representation view is C-compatible record layout for FFI
and native/AOT boundaries. Alignment controls follow as a constrained
extension. Packed records, endian-qualified scalar storage, and
structure-of-arrays (SoA) are deliberately deferred until the base
representation contract is proven across all backends.

This RFC is complementary to scalar opaque-newtype ABI transparency:
`opaque type UserId = Int` may reuse the existing scalar FFI representation,
but aggregate record layout remains a separate explicit boundary concern.

The load-bearing rule is:

> A representation annotation selects a physical view at an explicit boundary;
> it never changes the logical identity, equality, actor-message, persistence,
> or bytecode representation of a Nulang value.

## Motivation

Nulang currently has a deliberate separation between its semantic type system
and its runtime representation:

- the default VM uses the frozen tagged `u64` value layout
  (`src/value_layout.rs`, RFC 0001);
- bytecode records are runtime objects addressed through `RecMk` /
  `RecL` / `RecS`;
- the AOT backend lowers MIR through Cranelift and can already exploit
  compile-time type metadata;
- the C FFI maps scalar Nulang types through `src/ffi/marshal.rs`, but has
  no canonical language-level record ABI;
- the WASM backend has its own linear-memory ABI.

This is the correct architecture for Nulang, but it leaves systems-facing code
without a way to state an external layout contract. Odin demonstrates the
value of explicit layout control, while also illustrating why such control
must remain opt-in. Copying C layout into every Nulang record would couple
actors, persistence, the VM, and future hardware targets to today's native ABI.

The goal is therefore not "make Nulang structs C structs." The goal is "allow a
Nulang type to declare a checked physical projection when a boundary requires
one."

## Design

### 1. Semantic/physical split

A record keeps one logical type:

```nula
type Header = {
    kind: Int,
    length: Int,
}
```

A representation annotation adds metadata to that type declaration but does
not alter normal Nulang storage:

```nula
@repr(kind: "c")
type Header = {
    kind: Int,
    length: Int,
}
```

The compiler models this as:

```text
LogicalType(Header)
    |
    +-- VM view       -> ordinary tagged Nulang record (unchanged)
    +-- wire view     -> ordinary Nulang serialization (unchanged)
    +-- durable view  -> ordinary persistence schema (unchanged)
    +-- C view        -> computed ABI layout (new)
    +-- native view   -> backend may use C view only at an explicit boundary
```

No representation metadata participates in HM unification. Two otherwise
identical record types do not become type-compatible merely because their
physical views match.

### 2. Surface syntax

Phase 1 adds one declaration annotation:

```nula
@repr(kind: "c")
type Header = {
    kind: Int,
    length: Int,
}
```

The annotation grammar extends the existing declaration-annotation mechanism;
it does **not** add a keyword.

Accepted Phase-1 values:

- `"c"` — target C ABI field order/alignment.

Unknown representation kinds are a compile-time error. The compiler must never
silently treat an unknown representation as the default layout.

Phase 1 intentionally does **not** accept `packed`, custom byte offsets, or
endianness.

### 3. AST and HIR metadata

Add a representation descriptor in a backend-neutral module, for example:

```rust
pub enum ReprKind {
    C,
}

pub struct ReprSpec {
    pub kind: ReprKind,
    pub align: Option<u32>,
}
```

`ast::Decl::RecordType` and `hir::Decl::RecordType` gain
`repr: Option<ReprSpec>`.

The descriptor is declaration metadata. Ordinary expression `Type::Record`
values do not embed layout information, avoiding representation concerns in HM
unification and substitution.

### 4. Canonical layout engine

Create `src/layout.rs` as the single source of truth for external physical
layouts.

Proposed API:

```rust
pub struct FieldLayout {
    pub name: String,
    pub offset: u32,
    pub size: u32,
    pub align: u32,
}

pub struct RecordLayout {
    pub size: u32,
    pub align: u32,
    pub fields: Vec<FieldLayout>,
}

pub trait TargetLayout {
    fn scalar_layout(&self, ty: &Type) -> Result<ScalarLayout, LayoutError>;
}

pub fn record_layout(
    fields: &[(String, Type)],
    repr: &ReprSpec,
    target: &dyn TargetLayout,
) -> Result<RecordLayout, LayoutError>;
```

Neither the FFI layer nor an individual backend may independently recalculate
record offsets. Native, FFI, diagnostics, and tests all consume this canonical
layout engine.

### 5. C-view field types

Phase 1 is deliberately scalar-only. A `repr(c)` record field must have one
of the existing, concrete Nulang native-FFI projections below:

| Nulang field type | C-view ABI slot | Default libffi type | Size/alignment rule |
|---|---|---|---|
| `Int` | signed 64-bit slot | `i64` | 8-byte width; target ABI i64 alignment |
| `Float` | C `double` | `f64` | 8-byte width; target ABI f64 alignment |
| `Bool` | one-byte 0/1 value | `u8` | 1 byte / 1 byte |
| opaque wrapper over one of the above | same as underlying type | same as underlying type | same as underlying type |

This table is the target Phase-1 aggregate ABI and is derived from the existing
`FfiType -> CType -> libffi::middle::Type` path. Two details are
load-bearing:

1. **Int ABI width is not Int semantic width.** Nulang's tagged runtime stores
   a signed 48-bit integer payload with defined 48-bit wrapping arithmetic,
   even though its foreign call slot is `i64`. Copy-in is lossless for every
   Nulang Int. Copy-out from C must canonicalize the i64 through the same
   48-bit wrapping rule used by `Value::int`/the VM; the wider slot does not
   expand Nulang's logical Int domain. A future checked or full-width integer
   type would be a separate language feature.
2. **Bool ABI normalization is a prerequisite.** The libffi path currently
   uses an unsigned one-byte value, while the no-`ffi` fixed-arity fallback
   is expressed with Rust `bool`. Phase 1 requires one canonical one-byte
   0/1 foreign contract across both paths before aggregate layout is enabled.

Although scalar FFI calls already support `String` and `Unit`, they are not
Phase-1 aggregate fields:

- `String` maps to a C pointer and therefore needs an explicit ownership and
  lifetime rule when embedded inside a returned or mutable struct;
- `Unit` has no portable non-zero C field representation and C does not
  provide a standard zero-sized data member.

The following also remain rejected inside a Phase-1 `repr(c)` record:

- nested records and arrays;
- actors and actor references;
- closures/functions;
- arbitrary variants;
- GC-managed references whose lifetime cannot cross the boundary safely;
- open/generic record fields without a concrete monomorphized layout;
- raw pointers until Nulang exposes a source-level pointer type with explicit
  unsafe/provenance rules.

A later RFC or accepted extension may recursively admit nested C-view records,
strings, or pointers after their ownership and marshalling contracts are
specified. Phase 1 must never box or pointer-lower an unsupported field
implicitly.

### 6. FFI integration

The first consumer is `src/ffi/marshal.rs`.

A `repr(c)` record passed across an `extern` boundary is materialized into
a temporary ABI buffer described by `RecordLayout`. The logical Nulang
record remains an ordinary managed record; the native buffer is a projection,
not an alias of actor-heap storage.

For Phase 1, the boundary semantics are:

1. **argument:** copy each supported scalar field from the logical record into
   the ABI buffer at the canonical offset;
2. **native call:** libffi receives the aggregate using the canonical field
   sequence/layout;
3. **return:** canonicalize every foreign scalar according to Nulang semantics
   (including the defined 48-bit wrapping rule for Int), then copy the returned
   C aggregate into a fresh logical Nulang record field-by-field;
4. **no borrowed aggregate pointers:** C never receives a pointer into the
   actor heap, and Nulang never retains a pointer into the temporary ABI
   buffer after the call.

Mutation through `T*` is intentionally out of scope because Nulang currently
has no source-level raw-pointer FFI type. When pointer syntax is eventually
added, in/out/inout ownership must be specified separately rather than
smuggled into by-value `repr(c)`.

The VM's `Value` representation does not change. The FFI metadata/runtime
will need a versioned aggregate signature representation; it must consume the
same `RecordLayout` produced by `src/layout.rs`, not independently
recompute offsets.

### 7. AOT integration

The AOT backend may use a representation view at an ABI boundary without
changing the semantic MIR type.

Inside ordinary Nulang code, AOT is free to keep its existing representation
or perform target-specific scalar replacement. `@repr(c)` is not a global
optimization barrier and not a requirement that every temporary use C layout.

A future optimization may keep a value in its native view across adjacent
FFI/native operations when escape analysis proves that no semantic observation
can distinguish the optimization.

### 8. WASM behavior

Phase 1 does not define `repr(c)` as a WASM linear-memory layout.

When compiling a program that requires a C representation boundary under a
backend that cannot provide one, compilation fails with a backend-specific
unsupported-representation diagnostic.

A future `repr(wasm)` view, if needed, must be a separate representation kind
rather than overloading C ABI semantics.

### 9. Alignment extension

After `repr(c)` is implemented and tested, add:

```nula
@repr(kind: "c")
@align(bytes: "64")
type CacheLine = { ... }
```

Rules:

- alignment must be a power of two;
- the requested alignment may increase, never decrease, the target ABI's
  natural alignment;
- the maximum supported alignment is target-defined and diagnosed;
- alignment affects only the selected physical view.

This is intentionally a separate annotation so `repr` answers "which layout
algorithm?" while `align` answers "what extra alignment constraint?"

### 10. Packed layout is deferred

`packed` is not part of Phase 1.

Packed fields create unaligned access, borrowing, atomicity, and FFI hazards.
Adding it safely requires:

1. explicit unaligned load/store MIR operations or guaranteed byte-copy
   lowering;
2. diagnostics preventing references to misaligned fields;
3. target-specific tests across native and WASM;
4. a decision on whether atomics are forbidden in packed views.

Until those exist, `@repr(kind: "packed")` must be rejected as unknown.

### 11. Endian-qualified storage is deferred

Endian is a property of a stored scalar view, not of Nulang's logical integer
type. A future design should prefer explicit storage types/views such as
`u32le` / `u32be` or serialization primitives rather than making arithmetic
values carry ambient endianness.

This keeps ordinary `Int` architecture-independent.

### 12. Structure of Arrays is a collection representation, not record layout

SoA should not be expressed as `@repr(soa)` on a record. The record type
describes one logical value; SoA describes how a *collection of values* is
stored.

A future surface should therefore be collection-oriented, for example:

```nula
let particles: SoA[Particle] = ...
```

or a standard-library/compiler-intrinsic equivalent.

The logical operations remain record-oriented while the physical storage is:

```text
Particle[] logically

x[]  y[]  z[] physically
```

A future MIR optimization may auto-select SoA only when escape analysis and
access-pattern analysis prove it semantics-preserving. Explicit `SoA[T]`
must remain available when users need a stable performance contract.

### 13. Diagnostics

The compiler should expose layout rather than making it invisible.

A follow-up CLI surface should support:

```bash
nulang layout Header --target x86_64-unknown-linux-gnu
```

and report:

```text
Header @repr(c)
size: 16
align: 8

field   offset  size  align
kind    0       8     8
length  8       8     8
```

The offsets shown are illustrative for a target where i64 has 8-byte
alignment. The Phase-1 scalar widths are normative; target-specific alignment
and resulting padding come from the canonical `TargetLayout`.

Machine-readable JSON should be available for bindgen/header-generation tools.

## Tier Classification

Experimental.

The annotation syntax is new language surface, but the representation view is
explicitly outside the Frozen VM/value/wire layouts. Acceptance must not modify:

- `VALUE_LAYOUT_VERSION`;
- existing opcode numeric values;
- NUL0 packet representation;
- default record object layout;
- persistence encoding for ordinary records.

If implementation discovers that any of those frozen surfaces must change,
this RFC is insufficient and a separate Frozen-tier migration RFC is required.

## Backwards Compatibility

Existing programs are unaffected because representation metadata is opt-in.

A program using a new representation annotation will require a compiler version
that recognizes the annotation. Unknown representation kinds are hard errors,
which is intentional: ABI contracts must never degrade silently.

Compiled `.nbc` compatibility is preserved as long as representation metadata
is either compile-time-only or added to versioned metadata in a backwards-
compatible way. If durable `.nbc` artifacts must retain `ReprSpec` for later
linking, RFC 0001's version/migration mechanism governs the metadata addition.

## Alternatives Considered

### Make all records C-layout

Rejected. It couples actor heap storage, GC, persistence, bytecode, WASM, and
future targets to the host C ABI and prevents backend-specific optimizations.

### Put layout into `Type::Record`

Rejected for Phase 1. Physical layout should not participate in HM unification
or infect every structural record expression. Declaration metadata provides a
clean boundary.

### Use only `opaque type` for ABI representation

Rejected for aggregates, but useful and complementary for scalars.

Opaque nominal identity and aggregate physical representation solve different
problems. Scalar opaque newtypes may intentionally reuse their underlying
primitive FFI ABI (for example `opaque type UserId = Int` uses the same
foreign integer slot) while remaining nominally distinct in Nulang. That does
not define field offsets, padding, aggregate calling convention, alignment, or
ownership for records. A type may therefore need nominal identity, a
representation view, both, or neither.

### Let each backend define its own offsets

Rejected. Divergent FFI/AOT/WASM layout calculations are an ABI bug generator.
One canonical layout engine must own the calculation.

### Add packed/endianness/SoA immediately

Rejected. They have distinct safety and semantic constraints. Shipping them
together would make the smallest useful representation feature dependent on
the riskiest ones.

## Open Questions

1. Should Nulang later expose fixed-width source aliases/types such as
   `I8/I16/I32/I64` and unsigned counterparts for richer C APIs? Phase 1
   does not depend on that decision because its scalar slots are explicitly
   fixed above.
2. Should `repr(c)` generic records be rejected until monomorphization, or
   permitted only after all type parameters are concrete?
3. Should header generation live in the compiler (`nulang header`) or a
   separate package built on the layout JSON API?
4. Should alignment metadata be accepted in the same language version as
   `repr(c)` or one version later?
5. After Phase 1, should nested `repr(c)` records be the next aggregate
   extension, or should pointer/ownership semantics land first?

These questions affect ergonomics and rollout, not the central
semantic/physical separation.

## Resolution

Pending.
