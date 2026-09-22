# Nulang Documentation Map

This page separates current implementation guidance from strategic design,
experimental work, and archived planning material. Nulang is alpha software;
documentation should state whether a claim is implemented, experimental,
planned, or historical rather than presenting all design material as live
behavior.

## Start here

- [../README.md](../README.md) — project overview, installation, current feature
  status, and stability tiers.
- [GETTING_STARTED.md](GETTING_STARTED.md) — first program, language basics,
  actors, durable entities, testing, and package tooling.
- [TUTORIAL.md](TUTORIAL.md) — guided application tutorial.
- [PITFALLS.md](PITFALLS.md) — common syntax and semantic mistakes.
- [../examples/README.md](../examples/README.md) — runnable examples.

## Semantic and implementation references

- [../SPEC2.md](../SPEC2.md) — language specification and detailed semantic
  notes. Pay attention to implementation-status annotations where a section
  mixes current behavior with target semantics.
- [SEMANTIC_STABILIZATION_CONTRACT.md](SEMANTIC_STABILIZATION_CONTRACT.md) —
  active pre-production semantic/backend/durability verification contract.
  When older architecture prose conflicts with this contract, use this
  contract for the current stabilization direction.
- [../ARCHITECTURE.md](../ARCHITECTURE.md) — implementation architecture plus
  target architecture. Its opening implementation-status note identifies which
  material is as-built versus aspirational.
- [PERFORMANCE_ANALYSIS.md](PERFORMANCE_ANALYSIS.md) — measured/implemented
  performance work and optimization status.
- [../CHANGELOG.md](../CHANGELOG.md) — dated implementation changes and
  stability labels.

## Durable and distributed runtime

- [FABRIC.md](FABRIC.md) — Experimental messaging/stream substrate.
- [RESP_CACHE_ARCHITECTURE.md](RESP_CACHE_ARCHITECTURE.md) — Experimental
  RESP-compatible cache architecture.
- [threat-model.md](threat-model.md) — security/threat analysis and mitigation
  status.

Durable application code should prefer ordinary actors plus `entity` for
long-lived domain state. RFC 0017 keeps `agent` and `workflow` as
Experimental ergonomic source forms, but requires them to lower to the same
canonical actor/state/effect runtime model. `database` remains Experimental.
Do not treat these higher-level declarations as independent runtime species.

## Backends

Current policy:

1. The register bytecode VM is the semantic reference implementation.
2. Cranelift JIT is an optimization tier over that semantic baseline.
3. WASM is the canonical portable/cloud target.
4. `--backend wasm-component` is Experimental and emits WIT alongside WASM
   for Component Model interop.
5. Native AOT is secondary until semantic parity is demonstrated.
6. WasmFX is Experimental.

Backend availability does not imply semantic parity. Use conformance and
differential evidence before documenting a backend as equivalent to bytecode.

## RFCs and strategic documents

- [../RFC/](../RFC/) contains proposals and accepted design records. Always read
  the status header; an RFC can describe intended semantics that are only
  partially implemented.
- [PRD.md](PRD.md) is a strategic draft. It is useful for product direction,
  but it is not an implementation contract.
- [PLAN.md](PLAN.md) is an implementation/planning document and may contain
  completed work mixed with remaining tasks.
- [archive/](archive/) contains historical design/review material. Archive
  documents are context, not current behavior.

## Stability language

Use the project tiers precisely:

- **Frozen** — published compatibility contracts explicitly designated Frozen.
- **Stable** — current pre-adoption source semantics governed by the project's
  compatibility process; not yet a promise of permanent pre-1.0 immutability.
- **Experimental** — may change or be removed and should be labeled as such.

The historical `1.0.0-frozen` metadata does not mean every current
source-language behavior is permanently frozen. See
[../RFC/0021-compatibility-before-freeze.md](../RFC/0021-compatibility-before-freeze.md)
and [../GOVERNANCE.md](../GOVERNANCE.md).

## Documentation maintenance rules

When changing runtime or language behavior:

1. Update the executable implementation and tests first.
2. Update `CHANGELOG.md` with the status of the change.
3. Update `SPEC2.md` when semantics change.
4. Update README/onboarding docs when user-visible syntax, CLI behavior,
   stability, or supported platforms change.
5. Keep target architecture explicitly labeled; do not silently turn roadmap
   prose into an implementation claim.
6. Prefer source paths, conformance tests, or executable validation over copied
   test counts and performance numbers that will quickly become stale.
