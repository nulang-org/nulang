# RFC 0019 — Semantic Closure Before Further Surface Expansion

**Status:** Proposed  
**Target:** Nulang 1.x pre-stability work  
**Scope:** effects, continuations, actor authority, protocol identity, durable execution, content identity, backend parity

## Summary

Nulang already implements most of the primitives that distinguish it from a conventional systems language: typed actors, algebraic effects, reference capabilities, durable workflows, content addressing, multiple execution backends, and distributed runtime machinery.

The highest-risk work is no longer adding primitives. It is making the existing primitives compose under one set of semantics.

This RFC defines a **Semantic Closure** milestone. Until its exit criteria are met, new language-surface features SHOULD be deprioritized unless they directly close one of the invariants below.

The milestone has seven contracts:

1. **Backend-invariant language semantics.** Supported execution backends must not silently implement different languages.
2. **Affine continuations.** A continuation may be resumed at most once.
3. **Actor turn isolation.** Suspending effects do not permit a second ordinary message turn to mutate the same actor state concurrently.
4. **Typed external authority.** Reference capabilities and external-resource authority are distinct concepts; external authority is deny-by-default and structurally represented.
5. **Protocol-typed actor identity.** Actor references have a protocol identity suitable for cross-node compatibility checks.
6. **Separated semantic/artifact identity.** Source identity, semantic identity, and compiled artifact identity are not conflated.
7. **Explicit durable side-effect semantics.** Durable steps define idempotency/commit/compensation behavior rather than implying rollback of external effects.

## Motivation

A language with many individually strong features can still be unsafe or unpredictable when feature interactions are underspecified. In Nulang, the highest-impact examples are currently:

- continuation resume is supported more completely by the bytecode path than by every native path;
- spawn expressions carry an authority field through AST/HIR/MIR, while the source syntax and lowering are not yet uniformly wired;
- external authority tokens are represented as strings in parts of the pipeline even though the runtime treats them as security decisions;
- actor references identify actors operationally, but protocol identity is not yet a first-class distributed compatibility boundary;
- existing content hashes serve several integrity/cache purposes that should become distinct identities as code mobility grows;
- durable workflows increasingly reject nondeterministic effects, but the contract for committed external side effects needs to be explicit.

These are not reasons to simplify Nulang into a conventional language. They are reasons to finish the semantics of the design Nulang already has.

## Terminology

### Reference capability

A static ownership/aliasing property such as `iso`, `trn`, `ref`, `val`, `box`, or `tag`.

It answers questions such as:

- may this reference be aliased?
- may this reference mutate the object?
- may this value cross an actor boundary?

### Authority grant

Permission to cross a security boundary, for example:

```text
Net::TcpOut(api.stripe.com:443)
Fs::Read(/uploads/**)
Secret::Read(STRIPE_KEY)
```

It answers:

- what external action may this computation perform?

Authority is deny-by-default.

### Effect

A requested operation whose interpretation may be supplied by a handler or runtime boundary.

An effect is not itself authority. A handler is not itself authority. Authority is checked when interpretation crosses an external security boundary.

## Contract 1 — Backend-invariant semantics

### Requirement

If a program is accepted for more than one supported backend, its observable language semantics MUST be equivalent across those backends, except for explicitly documented target limitations such as performance, address width, or unavailable host integrations.

A backend MAY reject a language feature only if that backend is explicitly classified as a restricted profile. It MUST NOT silently reinterpret it.

### Direction

Effect/continuation lowering SHOULD move as high in the compiler pipeline as practical:

```text
Typed HIR
   ↓
Effect / continuation lowering
   ↓
Canonical MIR or continuation/state-machine IR
   ↓
VM | JIT | AOT | WASM
```

The preferred end state is that backend code generators consume already-lowered continuation semantics rather than independently implementing algebraic effects.

### Current backend profiles

The bytecode VM is the reference implementation for user-defined effect handlers and explicit continuation resume.

Native AOT currently supports handler-mediated resume control flow for the supported MIR profile, but explicit `resume(expr)` lowering through `RValue::Resume` remains a restricted feature and MUST be rejected deterministically until native lowering is complete.

The plain WASM backend is currently a restricted profile for user-defined effect handlers and continuation resume. Its public backend boundary MUST reject handler/resume MIR before code generation rather than allowing defensive emitter stubs to return `nil`.

WasmFX provides real stack-switching suspension/resumption for supported built-in asynchronous effects, but user-defined effect-handler resume is still a restricted profile. It likewise MUST reject that MIR before CIR/code generation until the user-handler path has complete semantics.

### Acceptance tests

For each backend advertised as semantically complete:

- handled effect returns the same result;
- early-exit/non-resuming handler returns the same result;
- nested handlers select the same arm;
- continuation capture/resume returns the same result;
- one-shot violation is rejected consistently;
- durable-effect restrictions are backend-independent.

A differential test harness SHOULD run the same source program against every complete backend and compare structured results/errors. Restricted profiles MUST have blocking tests that verify unsupported constructs fail loudly rather than silently returning a different value.

## Contract 2 — Affine continuations

### Requirement

Continuation values are affine: they may be consumed zero or one time, never more than once.

Conceptually:

```text
resume : Continuation[T] × T -> U
```

consumes the continuation.

A program that can resume the same continuation twice MUST be rejected statically when provable, or trapped deterministically before a second resume if dynamic structure prevents a static proof.

### Why affine rather than multi-shot

One-shot continuations substantially simplify:

- ownership and reference-capability interactions;
- actor turn isolation;
- resource lifetimes;
- native/JIT lowering;
- durable replay;
- FFI boundaries;
- stack/continuation allocation strategies.

Multi-shot continuations can be reconsidered later as an explicit opt-in abstraction if a compelling use case justifies the cost.

### Existing implementation direction

The runtime bytecode paths already consume captured continuation state with move-like semantics and trap if `Resume` executes without a captured continuation. That dynamic behavior is part of the affine safety net, not merely an allocation optimization.

The current `single_shot` analysis remains useful for choosing the lightweight `SingleShotState` fast path, but Semantic Closure requires the one-shot rule to remain true independently of whether that optimization proves a handler single-shot. Static analysis should reject provable duplication when Nulang exposes a path that can duplicate a continuation; dynamic consumption remains the backstop for cases that cannot be proven statically.

## Contract 3 — Actor turn isolation during suspension

### Requirement

An ordinary actor behavior executes as one logical turn. If that turn suspends on an effect, signal, durable wait, or other suspension point, a second ordinary message MUST NOT concurrently mutate the same actor state unless the actor/type explicitly opts into a reentrant model with separately specified invariants.

Default semantics are therefore non-reentrant across suspension.

### Required tests

- suspend a turn, enqueue another mutating message, verify it cannot observe an intermediate state;
- resume the first turn, verify deterministic ordering;
- crash/recover a suspended durable turn, verify the same ordering contract;
- verify monitor/link/system messages that are allowed during suspension cannot violate user-state isolation.

## Contract 4 — Typed external authority

### Requirement

Reference capabilities and external authority MUST be represented separately in compiler/runtime terminology and data structures.

Source-level spawn authority should be explicit, for example:

```nulang
spawn Worker() with [
  Net::TcpOut("api.stripe.com:443"),
  Fs::Read("/uploads/**")
]
```

The semantic rule is fixed:

- no grant means no external authority;
- grants are structural values, not unchecked runtime strings;
- lowering must preserve the grant exactly through AST → HIR → MIR → runtime metadata;
- runtime host functions check the structural grant before external access;
- delegation/attenuation may remove authority but must not manufacture greater authority without an authorized parent source.

### Migration

`src/authority.rs` provides the typed migration boundary while existing metadata still uses canonical tokens. The actor runtime bridge parses the legacy `BTreeSet<String>` into `AuthorityManifest`, fails closed if any persisted token is malformed, exposes typed exact-grant checks, and enforces exact-subset monotonic delegation.

The migration sequence is:

1. parse source syntax into `AuthorityGrant` values;
2. use typed grants in AST/HIR/MIR;
3. encode canonical tokens only at stable serialization boundaries if required for backward compatibility;
4. populate bytecode spawn-authority metadata from MIR;
5. pass the selected spawn grant set through the VM callback into the child actor;
6. have runtime host functions call the typed actor authorization boundary before external access;
7. remove ad-hoc string matching from security-sensitive checks.

Current plumbing gaps are explicit: the parser still initializes spawn capabilities to an empty vector; MIR codegen currently destructures `capabilities: _`, so it drops even programmatically constructed grants instead of filling `CodeModule::spawn_capability_grants`; and the current `spawn_actor` callback API carries no spawn-PC/grant argument for installing those grants on the child. The existence of AST/HIR/MIR fields or bytecode metadata therefore must not be treated as end-to-end enforcement yet.

## Contract 5 — Protocol-typed actor references

### Requirement

Distributed actor references need a stable protocol identity in addition to an operational actor/node ID.

Target model:

```text
ActorRef<P> = {
  node_id,
  actor_id,
  protocol_id(P)
}
```

`ProtocolId` is derived from a canonical protocol schema, not source formatting.

A remote send/ask MUST validate compatibility before dispatch or deterministically reject the operation.

### Compatibility

The first implementation SHOULD require exact protocol identity. Schema-evolution/subtyping compatibility can be added later only with explicit rules.

## Contract 6 — Source, semantic, and artifact identity

### Requirement

Three different identities MUST be distinguishable:

```text
SourceId
  = hash(source/package input)

SemanticId
  = hash(canonical typed/lowered semantic representation
         + referenced semantic identities)

ArtifactId
  = hash(SemanticId
         + compiler/ABI version
         + target
         + backend
         + codegen-relevant flags)
```

### Consequences

- formatting-only source changes can change `SourceId` without changing `SemanticId`;
- x86-64, AArch64, and WASM builds share semantic identity but have distinct artifact identities;
- Nulang Cloud can cache artifacts by `(SemanticId, TargetProfile)`;
- migration/replay code can reason about semantic compatibility independently of a machine-code blob.

Existing hashes do not need an immediate breaking replacement. New metadata fields can be introduced additively, followed by a versioned migration.

## Contract 7 — Durable external side effects

### Requirement

A durable step must not imply that arbitrary external effects are rollbackable.

Every external side effect inside a durable flow must fall into one of these classes:

1. **Replay-safe pure/deterministic operation** — can be re-executed safely.
2. **Idempotent committed operation** — has a stable operation key/idempotency key and records commit state.
3. **Compensatable operation** — records completion and has an explicit compensation path.
4. **Forbidden operation** — rejected inside the durable region.

The compiler/runtime SHOULD make the classification explicit enough that a crash between external commit and local persistence does not silently duplicate work.

## CI and repository discipline

Semantic closure is ineffective if local success and CI success are different definitions.

`scripts/ci-local.sh` is the canonical developer/agent preflight entry point. `.github/workflows/ci.yml` should remain behaviorally aligned with it.

Required policy:

- `main` should remain green;
- all-target builds include benches/examples so struct/schema drift is caught;
- documentation examples that are marked executable must compile/run under current semantics;
- backend differential tests become blocking as each backend graduates to complete status;
- security-sensitive authority tests are blocking;
- format and correctness lints remain blocking rather than being weakened to accommodate rapid changes.

## Milestone exit criteria

The Semantic Closure milestone is complete when all of the following are true:

- [ ] Complete backends pass an effect/continuation differential suite.
- [ ] Native AOT either implements explicit continuation-resume semantics or is explicitly classified as a restricted backend for that surface.
- [ ] Continuations are enforced as affine/one-shot, with runtime consumption tests and static rejection where duplication is representable/provable.
- [ ] Actor suspension has tested non-reentrant turn semantics.
- [ ] Spawn authority syntax parses and round-trips through AST/HIR/MIR/runtime.
- [ ] Security-sensitive runtime authority checks consume typed grants/manifests rather than ad-hoc strings.
- [ ] Reference capabilities and external authority are distinct in docs/compiler naming.
- [ ] Actor references include protocol identity for distributed dispatch.
- [ ] SourceId/SemanticId/ArtifactId are separately represented or a versioned migration is merged.
- [ ] Durable external effects have replay/idempotency/compensation classifications and crash-window tests.
- [ ] `bash scripts/ci-local.sh --full` passes before merge candidates.
- [ ] `main` CI is green with no known stale executable examples.

## Non-goals

This milestone deliberately does **not** require:

- DPU/SmartNIC actor routing;
- CXL persistent-memory support;
- FPGA/ASIC execution units;
- new actor scheduling algorithms unrelated to semantic correctness;
- multi-shot continuations;
- generalized protocol subtyping;
- transparent arbitrary live-stack migration;
- additional AI-agent language syntax.

Those may be valuable later, but they should not expand the semantic state space before the existing core is closed.

## Recommended implementation order

1. Restore reproducible green CI and keep `scripts/ci-local.sh` aligned with it.
2. Finish spawn authority parsing/lowering/runtime enforcement using `AuthorityGrant`.
3. Pin backend profiles with differential/restriction tests and eliminate silent semantic reinterpretation.
4. Make affine continuation consumption an explicit tested invariant and add static rejection wherever duplication is representable.
5. Specify and test actor suspension/non-reentrancy.
6. Introduce `ProtocolId` and protocol-typed actor-reference metadata.
7. Split semantic identity from artifact identity.
8. Add durable external-effect commit/idempotency crash-window tests.

Only after these gates are closed should major new surface-area work resume.
