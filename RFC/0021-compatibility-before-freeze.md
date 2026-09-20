# RFC 0021: Compatibility Before Permanent Freeze

- **Status:** Draft
- **Tier:** N/A (governance and compatibility policy)
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-09-20
- **Resolved:** TBD
- **Language-version at effect:** TBD on acceptance
- **Supersedes:** parts of RFC 0001 and RFC 0002 only if accepted
- **Superseded by:** none

## Summary

Nulang should preserve compatibility obligations without permanently freezing
pre-adoption implementation and source-language decisions too early.

This RFC proposes three rules:

1. **Freeze semantics only after evidence.** Nulang Core remains the minimal
   portability/self-hosting kernel, but its source-language semantics remain
   Stable rather than permanently Frozen until an explicit external-adoption
   gate is met.
2. **Version representations instead of freezing them.** Published bytecode,
   wire, value-ABI, behavior-manifest, and durable-state formats create
   long-lived read/migration obligations. Future encodings may evolve behind
   explicit version numbers and compatibility adapters.
3. **Preserve every published obligation deliberately.** A previously emitted
   artifact or protocol version must remain readable, migratable, or fail with
   an explicit compatibility diagnostic. No accepted semantic or format break
   may be silent.

The intent is to keep user trust and long-term artifact survivability while
avoiding irreversible constraints before Nulang has meaningful external use.

## Motivation

Nulang currently describes itself as alpha/pre-external-user software while
also publishing a `1.0.0-frozen` compatibility contract. That tension is now
material.

Recent semantic stabilization work has already exposed cases where a
correctness improvement can conflict with a rule classified Frozen. The
compiler can work around individual cases with warnings, strict modes, or
future-major-version plans, but repeatedly doing so turns a premature policy
choice into architectural debt.

At the same time, simply dropping compatibility promises would be worse.
Nulang's long-term value depends on programs, durable state, artifacts, and
wire protocols remaining understandable decades after their original
implementation changes.

The project therefore needs to distinguish four things that are currently easy
to conflate:

- **program meaning** — source semantics users intentionally depend on;
- **public compatibility formats** — emitted artifacts/protocols that may live
  longer than the compiler that produced them;
- **runtime ABIs** — versioned boundaries between compiler/runtime/Cloud;
- **implementation representations** — opcode selection, in-memory value
  layout, JIT internals, optimizer IR details, queue structures, and other
  mechanisms that should remain free to evolve.

The correct longevity contract is not "the implementation never changes." It is
"old meaning and old persisted/public data remain interpretable under an
explicit compatibility contract."

This RFC implements the policy discussion requested by issue #362 and builds on
the external validation gate in `docs/SEMANTIC_STABILIZATION_CONTRACT.md`.

## Design

### 1. Stability applies to semantics, not implementation choices

Nulang's compatibility policy SHALL distinguish semantic contracts from
representations.

Examples of **semantic contracts** include:

- the evaluation result of a valid Core expression;
- the typing/effect meaning of a public construct;
- actor message/protocol behavior declared stable;
- durable effect replay semantics;
- source-level capability and authority rules;
- migration and recovery semantics.

Examples of **representation choices** include:

- register allocation;
- bytecode opcode numbering;
- NaN/tagged-word bit layout;
- JIT calling convention internal to one runtime build;
- MIR/HIR node layout;
- mailbox data structure;
- snapshot compression;
- internal scheduler queues.

Representation choices MAY change when the enclosing public format/ABI version
changes or when they are not externally persisted at all.

No implementation representation becomes permanently immutable merely because
the source semantics above it are stable.

### 2. Reclassify Nulang Core as Stable until the adoption gate

RFC 0002's definition of **which constructs constitute Nulang Core** remains
useful and SHALL remain the portability/self-hosting kernel.

If this RFC is accepted, however, the current Core syntax, typing, and
evaluation rules SHALL be reclassified from permanently Frozen to **Stable**
until the adoption gate in section 6 is satisfied.

During this pre-freeze period:

- a Core semantic change still requires an RFC;
- no semantic break may be released silently;
- incompatible changes require a language-version transition;
- migration tooling or mechanical migration guidance is required when
  practical;
- existing external users/artifacts are compatibility obligations;
- implementation convenience alone is not sufficient justification.

The difference is that a demonstrated semantic defect MAY still be corrected
before permanent freeze through an explicit major-version/RFC process.

After the adoption gate is met, freezing Core requires a separate RFC that
lists the exact surface being frozen. Time alone does not freeze a surface.

### 3. Replace "frozen representation" with versioned archival compatibility

Public persisted/wire formats SHALL use explicit versioned compatibility
contracts.

The important promise is:

> A published version remains readable, migratable, or explicitly rejected
> with a precise compatibility error.

The promise is not:

> All future artifacts use the same byte representation.

This rule applies to at least:

- `.nbc` artifacts;
- NUL0;
- value ABI;
- Behavior Manifest;
- durable state/schema metadata;
- effect journal/history formats;
- compiler/runtime host-effect ABI where persisted or independently deployed.

A versioned surface SHOULD follow this model:

```text
old bytes
   ↓
version-specific decoder
   ↓
canonical semantic representation
   ↓
current runtime/compiler
```

or, where a migration is safer:

```text
old version
   ↓
validated migration chain
   ↓
new version
   ↓
current runtime
```

An implementation MAY retain a direct legacy execution path if that is simpler
and safer than migration.

### 4. Previously published versions create archival obligations

Once an artifact/protocol/ABI version has been released externally, the project
SHALL record its compatibility status.

Recommended statuses are:

- **Current** — emitted by current tools;
- **Readable** — no longer emitted, but directly readable;
- **Migratable** — requires a deterministic migration step;
- **Retired-with-diagnostic** — intentionally unsupported for a documented
  reason and fails with an explicit diagnostic.

For formats that contain durable user data or executable artifacts, retiring a
version without a migration path requires a dedicated RFC describing why the
data cannot safely be interpreted.

Unknown versions MUST fail closed. They MUST NOT be guessed, coerced, or parsed
as the nearest known version.

### 5. Version boundaries must be explicit and independently evolvable

The following versions SHOULD remain independently represented rather than
being inferred from one global language version:

```text
language_semantics_version
artifact_format_version
value_abi_version
wire_protocol_version
behavior_manifest_version
host_effect_abi_version
durable_history_version
state_schema_version
metering_semantics_version
```

A language release may update zero, one, or several of these.

This prevents an optimizer or transport improvement from requiring a source
language version change and prevents a source-semantic revision from silently
changing persisted/runtime interpretation.

### 6. Adoption gate for permanent source-semantic freezing

No additional source-language surface SHALL become permanently Frozen until the
project has evidence that the semantics are understood outside maintainer-owned
examples.

The minimum initial gate is:

1. **External applications:** at least five independently authored,
   non-maintainer applications exercising several runtime categories described
   in `docs/SEMANTIC_STABILIZATION_CONTRACT.md`.
2. **Durability proof:** the representative durable application survives the
   project's deterministic fault-injection/destructive recovery gate without
   unresolved semantic, authority, or replay violations.
3. **Migration evidence:** at least one real version transition has exercised
   source migration and at least one persisted/public format migration or
   backwards-reader path.
4. **Tooling evidence:** compiler, LSP, package manager, debugger/replay tools,
   and diagnostics have been used on those applications with documented
   friction.
5. **Compatibility inventory:** the candidate Frozen surface is enumerated
   mechanically or in checked-in documentation, with no accidental
   implementation details included.
6. **Freeze RFC:** a dedicated RFC names the exact semantics to freeze and
   records the evidence above.

Meeting the gate does not automatically freeze anything.

### 7. Compatibility precedence

When compatibility concerns conflict, use this precedence:

1. prevent silent corruption or authority violation;
2. preserve durable user data and acknowledged external effects;
3. preserve published artifact readability/migration;
4. preserve source compatibility;
5. preserve performance characteristics where documented;
6. preserve implementation representation only when required by an explicit
   external ABI.

A compatibility guarantee MUST NOT force the runtime to silently accept data it
cannot interpret safely.

### 8. Language-version naming

If accepted, the project SHOULD stop using `1.0.0-frozen` as the active
pre-adoption development designation.

The exact replacement is an implementation follow-up, but it MUST communicate
both facts truthfully:

- Nulang remains pre-production/alpha;
- compatibility changes are disciplined and versioned, not arbitrary.

Acceptable examples include a pre-1.0 semantic version or an explicit
compatibility-candidate suffix. The naming decision MUST NOT rewrite already
published artifact metadata; old version strings remain recognized historical
values.

### 9. Implementation changes are follow-up work

Accepting this RFC SHALL NOT itself alter parser/typechecker/runtime semantics.

After acceptance, follow-up PRs SHOULD update:

- `GOVERNANCE.md`;
- RFC 0001 / RFC 0002 status notes;
- `README.md` project-status wording;
- `SPEC2.md`;
- changelog/stability tables;
- language-version metadata;
- format/ABI compatibility inventory;
- migration/version diagnostic tests.

Each semantic change still requires its own justification under the revised
policy.

## Tier Classification

This RFC changes governance classification rather than adding language syntax.

If accepted:

- current Nulang Core moves from Frozen to Stable pending the adoption gate;
- previously published format/protocol/ABI versions retain archival
  compatibility obligations;
- future internal representations remain implementation details unless they
  cross a documented public/persisted ABI boundary;
- future permanent freezing requires an evidence-bearing RFC.

Existing Stable and Experimental surfaces remain unchanged unless separately
reclassified.

## Backwards Compatibility

This RFC is designed to increase practical compatibility rather than weaken it.

It does not invalidate any existing source program or artifact.

Existing emitted artifacts and protocol versions remain compatibility
obligations. Existing `1.0.0-frozen` metadata remains parseable as historical
metadata even if the active development version is renamed later.

The policy permits future explicit semantic correction before the adoption gate,
but only through an RFC, language-version transition, diagnostics, and migration
story. It does not permit silent churn.

## Alternatives Considered

### A. Keep RFC 0001/0002 unchanged forever

Rejected as the recommended direction.

It maximizes short-term nominal stability but risks preserving semantic mistakes
and implementation constraints before the language has enough external evidence
to know which details users actually depend on.

### B. Drop all compatibility guarantees until 1.0

Rejected.

Nulang already emits artifacts, durable metadata, and protocols whose
interpretation matters. "Alpha" is not a justification for silent corruption or
unreadable user state.

### C. Keep Core Frozen but unfreeze only formats

Rejected.

The motivating tension includes source-semantic rules themselves. Versioning
formats alone does not provide a disciplined way to repair an incorrectly
frozen semantic rule.

### D. Introduce a new permanent "Provisional" stability tier

Not required initially.

Stable plus an explicit adoption gate provides the needed semantics without
forcing every tool and document to learn another permanent tier. A future RFC
may introduce an additional tier if practical experience shows value.

### E. Freeze only an even smaller semantic micro-kernel now

Deferred.

It may eventually be correct to permanently freeze a smaller kernel than RFC
0002 currently defines, but selecting that kernel should use adoption and
self-hosting evidence rather than another pre-user guess.

## Open Questions

- What active language-version string should replace `1.0.0-frozen` if this
  RFC is accepted?
- Which already-published `.nbc`, NUL0, and value-ABI versions have actually
  escaped maintainer-only environments and therefore require archival fixtures?
- Should compatibility fixtures be stored directly in this repository or in a
  separate long-lived corpus?
- After the adoption gate, should Core freeze as one unit or should individual
  semantic areas freeze independently?
- What retention policy should apply to runtime-only ABIs that were never
  persisted and never crossed independently deployed component boundaries?

## Resolution

TBD.

## References

- Issue #362 — review Frozen Core compatibility policy before external adoption
- Issue #387 — external-use validation cohort
- RFC 0001 — Format Stability
- RFC 0002 — Frozen Core
- RFC 0010 — 100-Year Language Architecture
- RFC 0020 — Behavior Manifest
- `docs/SEMANTIC_STABILIZATION_CONTRACT.md`
- `GOVERNANCE.md`
- `SPEC2.md`
