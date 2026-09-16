# RFC 0022: Runtime Authority Capabilities

- **Status:** Draft
- **Created:** 2026-09-16
- **Owners:** Nulang runtime/compiler
- **Depends on:** RFC 0017 unified runtime primitives

## Summary

Nulang already has two different capability concepts and should keep them separate:

1. **Reference capabilities** (`iso`, `lineariso`, `trn`, `ref`, `val`, `box`, `tag`) are compile-time aliasing, mutation, ownership, and sendability rules.
2. **Runtime authority capabilities** are explicit grants that authorize an actor to perform external or privileged operations such as outbound network access.

This RFC completes the second system by finishing the existing `spawn Foo() with [...]` pipeline instead of introducing a second token format or a new capability lattice.

The intended invariant is:

> An actor may exercise runtime authority only when the authority was explicitly granted to that activation (or inherited through a policy-defined attenuation path), and the same authority decision is enforced regardless of interpreter, JIT/AOT, local placement, or remote placement.

## Current implementation state

The repository already contains most of the data model:

- `ast::Expr::Spawn` has `capabilities: Vec<String>`.
- HIR and MIR preserve `capabilities` on spawn rvalues.
- `CodeModule` has `spawn_capability_grants`, keyed by spawn instruction byte offset.
- `Actor` has a `BTreeSet<String>` capability manifest.
- Error documentation already defines runtime capability-denied behavior for tokens such as `Net::TcpOut(host:port)`.
- The CLI/effect checker has a separate process-level resource grant gate for categories such as `fs`, `net`, and `os`.

However, the end-to-end source-to-runtime path is incomplete in three places:

1. **Parser gap.** The spawn parser currently initializes the capability list to `Vec::new()` and explicitly notes that `with [Net::TcpOut(...)]` is not parsed yet.
2. **Codegen gap.** MIR bytecode generation currently destructures `RValue::Spawn` with `capabilities: _`, dropping the authority list rather than populating `CodeModule::spawn_capability_grants`.
3. **Activation-installation gap.** The VM/runtime spawn path does not yet consume the per-spawn side table and install its grants on the new actor activation.

A partial fix to only one of these layers is insufficient.

## Goals

- Make spawn-time runtime authority grants usable from Nulang source.
- Preserve the existing canonical token representation.
- Keep reference capabilities and runtime authority orthogonal.
- Make default authority explicit and least-privilege.
- Preserve authority across local execution, suspension/resume, supervision restart, hibernation/rehydration, migration, and remote spawn according to declared policy.
- Support attenuation without ambient authority escalation.
- Provide an audit trail for grants, denials, delegation, and revocation.

## Non-goals

- Replacing the reference-capability lattice.
- Encoding cloud IAM policy directly in the language.
- Treating compile-time effect rows as runtime authorization.
- Claiming cryptographic cross-node authority until the distributed token format is versioned and authenticated.
- Adding blanket implicit inheritance from a parent actor.

## Canonical authority token

Phase 1 keeps the existing canonical string representation because it already flows through AST/HIR/MIR metadata and is human-auditable:

```text
Net::TcpOut(api.stripe.com:443)
Fs::Read(/data/models)
Storage::Read(bucket/model-shards)
Inference::Use(model:gpt-5.6)
```

The runtime must treat the token as an opaque canonical authority identifier. Individual effect adapters interpret only token families they own.

The representation may later become a versioned structured encoding, but that migration must preserve canonical comparison semantics.

## Source syntax

The existing intended syntax becomes normative:

```nulang
spawn BillingWorker() with [
    Net::TcpOut("api.stripe.com:443")
]
```

Parsing must canonicalize each grant into the existing token string form and reject malformed or unsupported grant expressions at compile time.

## Compiler pipeline

The required lowering path is:

```text
source spawn ... with [...]
        │
        ▼
AST Spawn.capabilities
        │
        ▼
HIR Spawn.capabilities
        │
        ▼
MIR Spawn.capabilities
        │
        ▼
CodeModule.spawn_capability_grants[(spawn_pc, grants)]
        │
        ▼
VM Spawn at spawn_pc
        │
        ▼
Runtime activation.capabilities
```

### Codegen rule

When codegen emits a local `Spawn` instruction and the MIR capability list is non-empty, it records the same instruction byte offset used by `spawn_init_overrides` together with the canonical grant list.

The capability list is metadata for the logical spawn operation, not a mutable VM register payload.

### Remote spawn rule

Remote spawn must carry the grants in a versioned authenticated spawn request. Until that wire contract exists, remote spawn with non-empty authority grants must fail explicitly rather than silently dropping them.

## Runtime model

Each activation owns an authority manifest:

```text
ActivationAuthority {
    grants: Set<CapabilityToken>
    generation: ActivationGeneration
    policy_version: u32
}
```

The existing `Actor.capabilities` set is the Phase 1 storage location.

### Default

An actor spawned without explicit grants receives no external runtime authority beyond operations designated as universally safe runtime primitives.

Empty must mean **no grants**, not unrestricted access.

### Restart

Supervisor restart must reconstruct the activation from its spawn template and reinstall the same grant manifest. Restart must not accidentally widen authority.

### Hibernation / rehydration

Authority belongs to the logical activation policy, not volatile heap state. Rehydration restores the effective grant set before user code resumes.

### Migration

Migration preserves the grant set, subject to target-node policy. A target that cannot enforce a required authority must reject placement/migration rather than run with degraded enforcement.

## Enforcement boundary

Authorization belongs immediately before privileged host effects, not in arbitrary user code.

Conceptually:

```text
perform Net.connect(host, port)
        │
        ▼
resolve canonical required token
        │
        ▼
current activation authority manifest
        │
   authorized?
      /   \
    yes    no
     │      │
 host op   capability-denied error + audit event
```

Compile-time effect checking and runtime authorization both apply:

- **Effect row:** may this code perform this class of effect?
- **Runtime authority:** may this activation exercise this specific external authority now?

Neither substitutes for the other.

## Attenuation and delegation

A future authority value may support delegation, but authority can only stay equal or become narrower.

Example target surface:

```nulang
let stripe = authority(Net::TcpOut("api.stripe.com:443"))
let child = spawn BillingWorker() with [stripe]
```

Delegation rules:

- no grant can be manufactured without an authorized issuer;
- child authority is an explicit subset of the delegator's transferable authority;
- attenuation is monotonic;
- delegation events are auditable;
- cross-node delegation requires authenticated serialized authority.

Reference capabilities continue to govern whether an authority value/reference itself can alias or move; they do not define what resource the authority permits.

## Revocation

Phase 1 spawn manifests are static for the lifetime of an activation generation.

Phase 2 introduces revocation through a runtime authority registry:

```text
AuthorityId -> { token, issuer, generation, expires_at, revoked }
```

For distributed revocation, denial on uncertainty is required for high-risk authorities. Revocation consistency guarantees must be stated per authority family rather than implied as globally instantaneous.

## Observability

Every authority decision should be attributable through structured telemetry:

- actor logical identity / activation id
- authority token family
- decision: granted / denied / revoked / expired
- operation name
- target resource (redacted where appropriate)
- correlation / causation id when available
- node and placement
- policy version

Sensitive token payloads must not be blindly emitted into logs.

## Failure semantics

Authority failure is explicit. No privileged operation may silently downgrade into a no-op or unrestricted fallback.

Recommended categories:

```text
CapabilityDenied
CapabilityExpired
CapabilityRevoked
CapabilityUnavailableOnTarget
CapabilityDelegationDenied
```

## Implementation phases

### Phase 0 — finish the existing local path

1. Parse `spawn ... with [...]` into canonical `AST::Spawn.capabilities`.
2. Preserve current HIR/MIR propagation.
3. Populate `CodeModule::spawn_capability_grants` during bytecode generation.
4. Make VM `Spawn` resolve grants by spawn PC.
5. Install grants on the newly created actor before it becomes runnable.
6. Add end-to-end tests proving granted and ungranted operations diverge.

### Phase 1 — lifecycle correctness

1. Store grants in supervisor child restart templates.
2. Preserve grants through hibernation/rehydration.
3. Preserve grants through local migration.
4. Include authority state in actor introspection.

### Phase 2 — distributed authority

1. Version spawn/message authority metadata.
2. Authenticate cross-node grant material.
3. Enforce target-node capability support.
4. Add attenuation/delegation.
5. Add revocation and expiry.

## Required tests

- parser accepts one and multiple grants;
- parser rejects malformed capability arguments;
- MIR/codegen preserves exact canonical grants;
- two spawn sites of the same actor type can receive different manifests;
- empty grant list means denied by default;
- granted network destination succeeds while a different destination is denied;
- supervisor restart preserves but never widens grants;
- hibernation/rehydration preserves grants;
- remote spawn with grants fails explicitly until wire support lands;
- no backend (interpreter/JIT/AOT/WASM host) can bypass authority checks;
- audit events distinguish compile-time capability errors from runtime authority denial.

## Security considerations

The most dangerous failure mode is **authority erasure**: the compiler accepts a constrained grant declaration, but a later execution layer silently drops it and executes with ambient host authority. Therefore every boundary must either preserve authority metadata or reject the operation.

The second dangerous failure mode is conflating reference capabilities with runtime authority. `val` or `iso` says nothing about whether a value may access the network, filesystem, billing API, or model provider.

## Decision

Nulang will complete and harden its existing runtime-authority pipeline rather than introduce a second capability mechanism. Reference capabilities remain a static ownership/sendability system; runtime authority is explicit activation policy enforced at privileged effect boundaries.
