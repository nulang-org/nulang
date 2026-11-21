# Nulang Architecture Roadmap — 2026 Q4 Addendum

> **Status:** Draft planning addendum. This does not override `GOVERNANCE.md` or accepted RFCs.
> **Created:** 2026-09-16
> **Scope:** Reconciles the architecture review with work already in flight across Nulang's stacked P0, semantic-identity, durability, authority, protocol, delivery, and typed-actor branches.

## Thesis

Nulang does **not** need another wave of language features. The repository already contains substantial implementation work for typed actor protocols, nominal actor identity, semantic/artifact identity, authority, durable schema recovery, durable external-effect receipts, delivery metadata, and rolling protocol compatibility. The priority is to land and compose that work without creating parallel abstractions or duplicate RFCs.

The target remains:

```text
actor identity
+ typed protocol
+ effects/capabilities/authority
+ semantic/artifact identity
+ exact durable replay semantics
+ receipt-backed external effects
+ versioned delivery/protocol evolution
+ isolated execution
+ semantic tooling
```

The immediate objective is **semantic closure and consolidation**, not feature count.

## Existing implementation stack — do not duplicate

### P0 runtime stabilization

- **#245** — integration base for authority, ownership, durable authority preservation, migration sender preservation, host-boundary enforcement, and related runtime soundness work.
- **#269 / issue #261** — fail-closed unknown actor behavior dispatch. Unknown names/ids must never alias behavior slot 0.
- **#314 / issue #270** — compiler-side nominal actor behavior identity for statically proven receivers and ambiguous dynamic local dispatch.
- **#315** — preserve canonical actor schema identity after spawn.
- **#316** — enforce runtime actor-schema ownership for name/id dispatch.
- **#323** — carry actor schema identity through durable snapshots, recovery, migration, grains, and node-loss/shadow paths.

These PRs already implement the core of the earlier recommendation to make actor communication receiver-owned and fail closed. New actor-dispatch work should extend this stack, not create a second implementation.

### Typed actor protocols and rolling compatibility

- **#288** — first phase of static actor protocol checking without new source syntax: behavior existence, arity, argument constraints, and known `ask` return typing.
- **#306** — runtime protocol revision/fingerprint compatibility for staged rolling upgrades, separating the revision an activation emits from the revisions it accepts.

The recommended future structural `ActorRef[P]` surface remains valid, but it should build on #288 plus the #269→#323 nominal/runtime-ownership stack and reuse the rolling-compatibility model from #306. Do not create a third protocol identity/versioning scheme.

### Versioned delivery semantics

- **#303** — versioned `DeliveryEnvelope` / `MessageMeta`, stable logical message identity, retry attempt tracking, deadlines, structured dead letters, ActorMessage-v2 prefix helpers, and durable inbox/dedup commit semantics.

Protocol identity/revision metadata should eventually ride the same versioned delivery seam rather than creating an independent ad-hoc actor-message envelope.

### Semantic, protocol, source, and artifact identity

- **#160** — semantic-closure branch defining `SourceId`, `SemanticId`, `ArtifactId`, `ProtocolId`, `ProtocolActorRef`, artifact identity manifests, authority manifests, and durable-effect identity/persistence contracts.
- **#257** — derives canonical semantic identity from backend-independent MIR and typed actor-state schema information.

Therefore, do **not** introduce a parallel `DefinitionId` hierarchy merely to recreate identity already represented by `SemanticId` / `ArtifactId` / schema identity. Any finer-grained definition identity should be derived from or namespaced under that existing model only when a concrete consumer requires it.

### Durable migration and execution identity

- **#275 / issue #266** — enforces migration purity as a stricter deterministic replay contract, including transitive helper calls and locally handled effects.
- **#279** — preserves versioned migration-chain manifests through HIR → MIR → compiled actor metadata and validates migration topology before artifact production.
- **#262** — target design for executing versioned state migrations during recovery using semantic/state-schema identity and fail-closed compatibility checks.
- **#323** — nearer-term persisted actor-schema ownership through current snapshot/recovery/migration paths.
- **#265** — durable branch views plus version-pinned branch manifests that already demonstrate the requirement to bind historical state to artifact/schema/runtime identity before future execution.

The remaining high-value durability gap is not "invent identity"; it is to **bind ordinary durable history to the existing semantic/artifact identity model, retain the historical artifact, and enforce exact compatibility or explicit migration during replay/upgrade**.

### Durable external effects

- **#317** — defines the runtime invariant for receipt-backed, replay-safe external effects using a stable logical effect occurrence identity rather than retry/wall-clock identity.
- **#347** — implements the current durable-turn v2 direction: persisted `DurableEffectContext`, fenced preparation, 0..N receipts per turn, atomic terminal-receipt + inbox/state + activation-fence commit, and replay of exact receipts.

This is already the concrete form of the earlier idea that durability should be an interpretation of effects. Do not invent another durable-effect abstraction. The remaining convergence is to use canonical semantic/durable owner identity, connect #303's `DurableInboxBundle`, unify activation fencing, and integrate real provider/host effect paths.

## P0 — finish semantic correctness

### P0.1 Land the stabilization chain in dependency order

Recommended order:

```text
#245
  -> #269  (#261 fail-closed dispatch)
  -> #314  (compiler nominal dispatch)
  -> #315  (runtime schema identity)
  -> #316  (runtime behavior ownership)
  -> #323  (durable schema identity)
```

Do not collapse this into one opaque merge while the repository's validation queue is saturated. Each child has a narrower invariant and should retain exact-head test evidence.

### P0.2 Rebase typed protocols onto the stabilized actor model

After the P0 chain above is stable, rebase/extract #288 so static protocol checking is consistent with the same nominal receiver identity used by runtime dispatch. Avoid maintaining one compiler protocol identity and a different runtime ownership identity.

Then integrate #306's protocol revision/fingerprint model with the canonical compiler-generated protocol metadata rather than maintaining hand-authored runtime fingerprints.

### P0.3 Exhaustiveness truth and implementation

Tracked by **#332**.

Public docs previously claimed all non-exhaustive matches are compile-time errors while the implementation still contains a runtime `non-exhaustive match` path. **#348** narrows the documentation claim immediately. The compiler follow-up should add real exhaustiveness analysis for closed variants/booleans/nested patterns and define guarded-arm coverage precisely.

### P0.4 Migration purity before executable upgrade

Land/review **#275** before treating state migrations as executable production upgrade machinery. A migration that can perform hidden or irreversible effects cannot be safely replayed, retried, branched, or used during node-loss recovery.

## P1 — exact durable semantic/artifact pinning

This is the main identity work that remains genuinely additive after accounting for #160, #257, #265, #275, #279, #262, and #323.

### Required invariant

A durable activation must never silently replay historical state/events under unrelated executable semantics merely because the source-level actor/entity name matches.

Persist enough existing identity to prove compatibility, conceptually:

```text
DurableExecutionIdentity {
    semantic_id,
    artifact_id,
    state_schema_id,
    state_schema_version,
    protocol_id/revision: optional,
    runtime_semantics_version,
    journal_format_version,
}
```

Use the actual types/encodings established by the semantic-closure and durable-manifest work; do not create duplicate hash classes.

### Recovery rule

1. Read persisted execution/schema/protocol identity.
2. Resolve an exact compatible artifact or prove semantic compatibility under a specified rule.
3. Verify actor/protocol/state-schema ownership.
4. Replay only under compatible semantics.
5. If code/schema changes, require the deterministic migration path from #275/#279/#262.
6. Record the successful identity transition exactly once after migration commits.
7. Fail closed if compatibility or the required historical artifact cannot be established.

### Artifact retention

Add content-addressed lookup/retention around the existing `ArtifactId` model so old durable instances can retrieve the code they are pinned to. Nulang Cloud may provide managed retention, but correctness must not depend on a proprietary service.

#265's version-pinned `BranchManifest` is useful precedent: consolidate the identity contract rather than creating a different manifest shape for ordinary durable replay.

## P1 — unify durable turns, effects, and delivery

Use **#317/#347** for external-effect occurrence/receipt semantics and **#303** for inbox/message identity. The two must converge on one durable turn commit contract:

```text
activation fence
+ durable actor sequence
+ inbox/dedup bundle
+ 0..N effect intents/receipts
+ actor state
+ semantic owner/effect-site identity
= one recoverable durable turn
```

High-priority follow-ups:

1. replace ad-hoc `actor:{id}` durable-effect ownership with canonical durable semantic owner identity;
2. consume #303's exact `DurableInboxBundle` instead of duplicating inbox/dedup state;
3. reuse the canonical activation-fence implementation rather than parallel fence logic;
4. carry a compiler-produced semantic effect-site identity through bytecode/native/WASM;
5. integrate one real host/provider effect path with crash-window tests before generalizing to payments/LLMs;
6. expose indeterminate external-effect states explicitly—never claim exactly-once provider execution without provider participation.

## P1 — unify protocol evolution with delivery

Use **#303** as the transport/delivery seam and **#306** as the rolling-compatibility policy.

Target composition:

```text
compiler actor protocol
    -> canonical ProtocolId / revision fingerprint
    -> activation ProtocolSupport (emit + accepts)
    -> versioned DeliveryEnvelope metadata
    -> receiver compatibility gate
    -> structured dead letter on mismatch
```

This avoids changing the wire format independently for every new protocol concern and preserves logical message identity across retry, spawn queues, behavior fetch, and migration.

## P1/P2 — finish authority enforcement

Existing authority work is substantial; the remaining task is enforcement coverage, not a new capability language.

Priorities:

1. ensure actual FS/network/env/secret/process/FFI host dispatch sites call the typed authority boundary;
2. ensure source spawn authority is preserved from parser/HIR/MIR through exact spawn-site provenance;
3. derive WASM/WIT/WASI imports/authority from the same effect/capability/authority model where practical;
4. close production WASM dispatch stubs before claiming full component sandboxing;
5. isolate untrusted plugins/agent-generated code with explicit runtime resource budgets;
6. test that bytecode/native/WASM paths cannot bypass equivalent authority checks;
7. preserve the same authority through recovery, migration, hot reload, and node-loss re-spawn.

Do not add a second sandbox configuration language unless the existing effect/capability/authority model demonstrably cannot express a required policy.

## P2 — semantic inspection and agent tooling

Tracked by **#336**.

Expose compiler-owned semantics rather than creating AI-specific source syntax:

```text
nula inspect <definition> --json
nula graph --dependencies <definition>
nula graph --affected-by <semantic-id>
nula artifact inspect <file.nbc>
nula durable inspect <instance>
nula protocol diff <old> <new>
```

The stable machine-readable schema should expose types, effects, capabilities/authority, actor/protocol identity, semantic/artifact IDs, protocol revisions, dependency edges, durable dependencies/migrations/effect sites, and source spans.

Agents should request semantic slices from this interface rather than repeatedly reconstructing the program graph from raw source.

## P3 — optional proof tooling

Reuse existing `requires` / `ensures`, formal semantics, effect rows, capability checks, migration purity, and semantic schemas.

A future:

```text
nula prove
```

should attempt static discharge and report explicitly:

```text
proved
unknown
counterexample
```

Initial useful targets are pre/postconditions, protocol compatibility, simple durable-state/migration invariants, and effect/capability/authority containment. Do not turn proof obligations into a requirement for ordinary Nulang programs.

## Branch strategy: extract, do not merge broad research branches wholesale

PR **#160** contains important foundations but is broad and predates much of the current P0 stack. Prefer extracting/rebasing its still-needed primitives into narrowly reviewable children of the stabilized base, as already demonstrated by **#257** and the authority work folded into **#245**.

The audit found that #160 still uniquely contains several useful primitives—content/artifact identity helpers, protocol-reference wire helpers, and the original durable-effect primitives—but those should be extracted only where newer focused work has not superseded them. In particular, #317/#347 should own durable-effect runtime semantics rather than replaying #160's older draft wholesale.

Likewise, prefer small dependency-ordered migration/protocol/delivery PRs over one giant "future architecture" merge. The repository already has the right pattern; keep using it.

## Ideas that should not become new roadmap epics

- **Effect polymorphism:** already present; improve inference, diagnostics, handlers, and tests.
- **Durability as an effect interpretation:** already concretely owned by #317/#347; finish integration rather than add a parallel abstraction.
- **Content addressing:** already present and substantially extended by #160/#257/#265; finish integration rather than inventing another ID family.
- **Protocol versioning:** #306 already defines a rolling compatibility model; generate it from compiler protocol identity rather than inventing another scheme.
- **Delivery identity/dead letters:** #303 already defines the compatibility seam; finish transport/runtime integration.
- **Rust/Mojo-style ownership:** do not add a second ownership model beside Nulang capabilities.
- **Knowledge/confidence types:** keep in libraries until production evidence demands type-system support.
- **Generic `Remote` effect:** experiment in libraries only; avoid competing with actor distribution.
- **AI-specific syntax:** agents should be supervised/durable actors plus libraries/effects/tools, not a second core language.

## RFC numbering / branch hygiene

Several open branches independently allocated the same draft RFC numbers before any of them landed on `main`. Do not add another competing RFC number in this roadmap branch. Reconcile/renumber draft RFCs when their owning implementation is rebased for merge. RFC numbers on unmerged branches are not a reason to create parallel architecture.

## Sequencing

```text
P0 semantic correctness
  #245 -> #269 -> #314 -> #315 -> #316 -> #323
                     + #332/#348 exhaustiveness truth
                     + #275 migration purity
                     + rebase #288 typed protocol checking
        |
        v
P1 durable/protocol/effect identity
  extract needed #160 identity primitives
  #257 canonical MIR semantic identity
  #279 migration manifests -> #262 executable deterministic migrations
  #303 delivery/inbox identity + #306 rolling protocol compatibility
  #317/#347 receipt-backed durable effects + atomic durable turn
  exact durable SemanticId/ArtifactId pinning + artifact retention
        |
        v
P1/P2 authority completion
  host dispatch + WASM/WIT/WASI enforcement
        |
        v
P2 semantic graph / inspection tooling
        |
        v
P3 optional proof tooling
```

## Stop conditions

Do not start a new language-surface feature while any of these remain true:

1. unknown actor messages can invoke unrelated behavior;
2. duplicate behavior names can cross actor schemas;
3. runtime numeric behavior ids can bypass target-schema ownership;
4. documented exhaustiveness differs from actual compiler guarantees;
5. migrations can perform replay-unsafe effects or lose their manifest through compilation;
6. durable replay can bind historical state to unproven executable semantics;
7. durable external-effect replay can duplicate or ambiguously observe a logical effect without an explicit recovery policy;
8. rolling protocol compatibility and delivery metadata disagree about the message being accepted;
9. a security claim can be bypassed at a supported execution/host boundary.

The purpose of this roadmap is to make Nulang's existing thesis stronger, smaller, and enforceable—not to maximize the number of features.