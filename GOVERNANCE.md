# Nulang Governance

> This document records the current maintainer policy for the Nulang
> programming language. The process it defines may evolve through the RFC
> mechanism it specifies.
>
> **Status:** Ratified 2026-07-19 and amended by accepted RFC 0021
> (Compatibility Before Permanent Freeze) on 2026-09-21.
>
> **Pre-adoption reality check:** Nulang is alpha software without meaningful
> external adoption yet. Published artifacts/protocols remain explicit
> compatibility obligations, but source semantics are not permanently Frozen
> until the evidence-bearing adoption gate in RFC 0021 is met. Semantic breaks
> still require an RFC, an explicit language-version transition, diagnostics,
> and a migration story; silent churn is never permitted.

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

Every public surface of Nulang is classified into one of three tiers. The tier
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
- The currently published `IO`, `Spawn`, `Send`, `Receive` built-in
  effect contracts remain Frozen pending a dedicated reclassification RFC.

### Stable
Breaking changes require an accepted RFC and a deprecation cycle of at least
two major versions (the deprecated surface remains functional, emits a
warning, and is removed only in the version after next).

- Nulang Core syntax and semantics (RFC 0002), reclassified from Frozen by
  RFC 0021 until the external-adoption freeze gate is satisfied.
- The full HM type system and inference rules.
- The effect-row system (closed/open, regions).
- The capability lattice (`iso/trn/ref/val/box/tag/lineariso`) and subtyping.
- The actor surface (`spawn`, `send`, `receive`, supervision).
- CRDT operations are **not Stable-tier language surface**. An August 2026
  erratum correctly removed an earlier unsupported Stable claim. Source-level
  typed `state crdt <type>` fields and `Crdt.*` operations were implemented
  later and are covered by conformance tests, but they remain **Experimental**
  under the unified runtime work (RFC 0017) until an accepted RFC explicitly
  promotes their source semantics.

### Experimental
No stability promise. May change or be removed in any release. Lives behind a
feature flag (`wasm-backend`, `python`, `sqlite`, `lsp`) or is explicitly
marked experimental in `CHANGELOG.md`. Current examples include multi-node
distribution, source-level CRDT state/replication, and the higher-level
`agent`/`workflow` runtime sugar governed by RFC 0017.

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
  and the `LANGUAGE_VERSION` const in `src/format/constants.rs`): moves only
  on RFC-ratified change to Frozen or Stable surfaces. Recorded in every
  `.nbc` artifact. A runtime rejects an artifact whose language version
  exceeds its own (`FormatError::IncompatibleLanguage`).

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
