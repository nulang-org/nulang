# Nulang Governance

> This document records the current maintainer policy for the Nulang
> programming language. The process it defines may evolve through the RFC
> mechanism it specifies.
>
> **Status:** Ratified 2026-07-19 and amended by accepted RFC 0021
> (Compatibility Before Permanent Freeze) on 2026-09-21.
>
> **Pre-adoption reality check (2026-10-17):** Nulang is alpha software without
> independent production adopters. Source-language Core semantics are now
> **Incubating**, not Frozen or Stable: deliberate changes can proceed without
> a two-major-version deprecation cycle or one RFC per revision. Every semantic
> change still needs an explicit specification, regression/conformance evidence,
> and versioning when a persisted or independently deployed boundary changes.
> Previously published artifact/protocol versions remain separately versioned
> and must not be silently reinterpreted. See RFC 0032.

## 1. Purpose

Nulang is engineered for long-term durability. No implementation, author, or
dependency lasts forever; the *process* must. This document specifies how the
language is changed, by whom, and under what stability guarantees — so that a
program written today continues to run decades from now under conforming
implementations that did not exist when it was written.

The lessons are drawn from languages that survived (C/ISO, Python/PEP, Rust/RFC,
Lisp/ANSI) and those that didn't (single-implementation languages that died
with their host). The recurring pattern: a published, versioned stability
contract and a process that makes breaking changes expensive and survivable.

## 2. Stability Tiers

Every public surface of Nulang is classified into one of four tiers. The tier
determines what changes are permitted and how. See `CHANGELOG.md` for the
current classification.

### Frozen
A published version in this tier is an archival compatibility obligation. Its
bytes/meaning are never silently reinterpreted; future encodings may evolve
under a new explicit version with a reader/migration path. Frozen therefore
applies to the named published version, not to internal implementation choices.

- `.nbc` bytecode format version 1 (`src/format/constants.rs`).
- NUL0 wire protocol version 1.
- Value layout version 1 (`src/value_layout.rs`).

### Incubating (pre-adoption language semantics)
Core syntax, evaluation, types, effects, capabilities, and built-in
`IO`/`Spawn`/`Send`/`Receive` source contracts may change while Nulang
has no independent production consumers. Changes must be intentional, reviewed,
documented in `CHANGELOG.md` and `SPEC2.md`, and covered by conformance and
cross-backend tests where relevant. A breaking change to a serialized format,
wire protocol, durable history, or public embedder ABI still needs a new
version or explicit migration/rejection path; incubation does not waive data
integrity or authority guarantees.

Incubating changes do **not** require a mandatory RFC, two-major-version
deprecation cycle, or artificial compatibility shim. The adoption gate in
RFC 0021 controls when any named semantic surface can be promoted to Stable
or Frozen. A substantial design may still use an RFC for review.

### Stable
Breaking changes require an accepted RFC and a deprecation cycle of at least
two major versions (the deprecated surface remains functional, emits a
warning, and is removed only in the version after next).

- Public surfaces explicitly designated Stable by their own accepted RFC,
  excluding the pre-adoption language Core and its immediate built-ins.
- ~~CRDT operations and their merge semantics~~ — **erratum (2026-08-02):**
  removed. This was never an accurate claim: there is no `.nula`-level
  syntax to construct or operate on any CRDT type today (no type
  selector, no `Crdt.*` effect module — `state crdt` fields behave
  identically to `state durable`; see `SPEC2.md` §9.10/§12.5). Since no
  conforming program could ever have relied on this surface, its removal
  is a documentation correction, not a stability-tier deprecation — there
  is nothing to deprecate. The Rust-level `CrdtManager` delta-sync
  protocol (`src/runtime/crdt.rs`, `crdt_reg.rs`) is real and tested, but
  is an embedder API, not a tiered language surface, until an RFC wires
  it to `state crdt` fields.

### Experimental
No stability promise. May change or be removed in any release. Lives behind a
feature flag (`wasm-backend`, `python`, `sqlite`, `lsp`) or is explicitly
marked experimental in `CHANGELOG.md`.

## 2a. Keyword Lifecycle (RFC 0010)

Every keyword in the language imposes a permanent tax on the user namespace.
The following rules govern keyword introduction, reservation, and removal:

1. **A keyword, once added to the Frozen or Stable tier, is permanent.**
   Its removal requires an RFC, a deprecation cycle (≥2 major versions),
2. **A reserved-but-unwired keyword has no tier.** As of language version
   1.0.0-frozen, five formerly-reserved keywords (`where`, `priv`, `loop`,
   `node`, `subworkflow`) have been removed from the lexer per RFC 0010 §C.6
   and now lex as plain identifiers. `await` has been **re-reserved** (July
   2026) for future async/await support and cannot be used as an identifier.
   Any keyword may be re-added with a proper RFC when its feature is
   implemented.

3. **New keyword introduction** requires an RFC specifying:
   - Which tier (Frozen, Stable, Experimental) the keyword occupies.
   - Whether it is a reserved word or contextual keyword.
   - The migration path if it shadows an existing identifier.

4. **Keyword deprecation:**
   - Emit a compiler warning on use for one major version.
   - Keep the keyword reserved (unusable as identifier) for one additional
     major version after functional removal.
   - Free the identifier in the following major version.
   - Migration tooling (`nulang migrate`) rewrites affected source files.

The canonical keyword inventory lives in `src/lexer.rs` §keyword_map and
is audited in `SPEC2.md` §Implementation Status.

## 3. Roles

### Language Steward
- Final authority on RFC acceptance or rejection.
- Responsible for ensuring accepted RFCs are implemented and the
  `CHANGELOG.md` tiers are kept accurate.
- May delegate review but not the acceptance decision for Frozen-tier changes.

### RFC Authors
- Any contributor may author an RFC. The steward is the default author of
  Frozen-tier RFCs.

### Implementers
- May fix bugs and add Experimental-tier features without an RFC.
- Must not change Frozen or Stable surfaces without an accepted RFC.
- May revise Incubating Core with tests, spec/changelog updates, and explicit
  artifact/ABI versioning where affected; a separate RFC is optional.
- Must record every user-visible change in `CHANGELOG.md` under the correct
  tier.

## 4. The RFC Process

1. **Draft.** Copy `RFC/0000-template.md` to `RFC/NNNN-short-name.md` (next
   free number). Fill in every section.
2. **Discussion.** The RFC is discussed in the project's issue tracker. The
   steward resolves blocking questions; non-blocking questions are recorded
   in the RFC's "Open Questions" section.
3. **Accept or Reject.** The steward accepts or rejects. The decision and its
   rationale are recorded in the RFC's "Resolution" section. A rejected RFC
   is kept in `RFC/` with its resolution; it is not deleted.
4. **Implement.** The accepted RFC is implemented. The implementation must
   include tests and a `CHANGELOG.md` entry.
5. **Ratify.** Once implemented and verified, the RFC's status changes from
   "Accepted" to "Implemented". Frozen-tier RFCs additionally record the
   language version at which they took effect.

An accepted RFC is **immutable**: its text is never edited after acceptance.
Corrections require a new RFC that supersedes it (recorded in the superseding
RFC's header). RFC 0021 is the accepted policy that supersedes the permanent
source-semantic freeze portions of RFC 0002 without rewriting that historical
RFC.

## 5. Versioning

- **Crate version** (`Cargo.toml` `version`): the implementation version. May
  rev freely (semver) for bug fixes, performance, and Experimental features.
- **Language version** (`Cargo.toml` `[package.metadata] language-version`,
  and the `LANGUAGE_VERSION` const in `src/format/constants.rs`):
  increment/identify the semantic generation when a released Incubating change
  makes prior compiled artifacts incompatible. Frozen and Stable changes
  additionally require an RFC. The version is recorded in `.nbc` artifacts,
  and newer incompatible versions are rejected rather than guessed.
  The historical string `1.0.0-frozen` denotes existing v1 metadata, not
  a promise that the active source Core is permanently frozen.

## 6. Deprecation

A Stable-tier surface that is to be removed is first marked `#[deprecated]`
in the implementation and noted in `CHANGELOG.md` with the version that
deprecated it and the version that will remove it. The deprecated surface
remains functional for at least one major language version. Removal requires
an RFC and a major-version bump.

## 7. Authoritative Artifacts

The language is defined by the interaction of three artifacts, in priority
order:

1. `SPEC2.md` — the prose specification. Explanatory.
2. `spec/formal/` — the machine-checked formal semantics (RFC pending). Where
   the formal model and the prose disagree, the formal model is authoritative.
3. `conformance/` — the behavioral conformance suite. Where an implementation
   and the spec disagree, the conformance suite is the oracle.

A conforming implementation passes the conformance suite and respects the
frozen format versions. Multiple conforming implementations are not just
permitted but expected and encouraged.
