# RFC 0020: Nulang Behavior Manifest

- **Status:** Draft
- **Tier:** N/A (tooling/deployment contract; no Core syntax change)
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-09-17
- **Language-version at effect:** N/A
- **Supersedes:** none

## Summary

Define a versioned, machine-readable **Nulang Behavior Manifest** emitted beside a compiled artifact. The manifest describes the program's externally relevant semantics without freezing compiler internals such as HIR or MIR.

The manifest is the contract between the Nulang compiler/runtime and deployment systems such as Nulang Cloud. It makes effects, external authority, durability, replay constraints, interfaces, state-schema identity, resource intent, and artifact provenance inspectable before execution.

This RFC intentionally proposes **no new language keyword and no change to Nulang Core**.

## Motivation

Nulang already has the semantic ingredients needed for long-lived autonomous software:

- actors and supervision;
- algebraic effects;
- reference capabilities and typed external authority;
- durable entities and workflows;
- explicit state and migrations;
- backend-invariant semantics;
- deterministic simulation and conformance testing;
- versioned bytecode and wire formats.

Today those facts are spread across source, inferred types/effect rows, compiler metadata, runtime configuration, deployment configuration, and documentation. A deployment platform therefore has to infer behavior from implementation details or rely on manually duplicated policy.

That becomes increasingly unsafe as implementation is generated or rewritten automatically.

The durable artifact should be a compact semantic contract that answers:

1. What executable artifact is this?
2. Which public actors/functions/interfaces does it expose?
3. Which effects can it perform?
4. Which external authorities can it require?
5. Which durable state schemas does it own?
6. Which effects are replay-safe, idempotent, or irreversible?
7. Which resource constraints or placement requirements are declared?
8. Which compiler/source/dependency inputs produced the artifact?

The manifest is not a proof that the program is correct. It is a compiler-produced summary that deployment, admission, policy, auditing, and tooling can validate against stronger organization/platform policy.

## Non-goals

This RFC does **not**:

- freeze HIR, MIR, optimizer IR, VM internals, or backend-specific layouts;
- add natural-language intent as an executable authority source;
- add new Core syntax;
- claim network-wide exactly-once semantics;
- replace WIT, NUL0, package manifests, signatures, SBOMs, or provenance attestations;
- define Cloud-specific scheduling policy;
- require every field below in the first implementation.

## Design principles

### 1. Semantics over mechanisms

The manifest describes stable behavior such as effects and authority rather than transient implementation choices such as TCP, NATS, Firecracker, Postgres, or a specific inference provider.

### 2. Compiler-derived beats duplicated configuration

Anything the compiler can derive soundly from typed source should be emitted automatically. User-supplied policy can further restrict the manifest but must not silently widen inferred authority/effects.

### 3. Fail closed on unknown semantics

Consumers must reject unknown required semantic versions or fields whose meaning is necessary for enforcement. Forward-compatible advisory fields may be ignored only when explicitly classified as advisory.

### 4. Backend invariant

A supported backend must produce behavior metadata equivalent to other supported backends for the same source semantics. Backend-specific details may live under an explicitly non-semantic extension namespace.

### 5. Artifact identity is content-derived

The manifest binds to the executable artifact by cryptographic digest. A manifest for artifact A must never authorize artifact B.

## Artifact layout

For a package build, the proposed initial layout is:

```text
.nula/dist/<package>.wasm
.nula/dist/<package>.behavior.json
```

Other backends may emit a different executable extension while retaining the `.behavior.json` sidecar.

A future package/container format may bundle both without changing manifest semantics.

## Schema identity

Initial experimental media type / schema identity:

```text
schema: nulang.behavior/v0alpha1
```

The schema is experimental until separately stabilized. Schema versioning is independent from the Nulang language version and executable format version.

## Required top-level fields

Conceptual initial shape:

```json
{
  "schema": "nulang.behavior/v0alpha1",
  "package": {
    "name": "payments",
    "version": "0.4.0",
    "language_version": "1.0.0-frozen"
  },
  "artifact": {
    "kind": "wasm-module",
    "digest": "blake3:..."
  },
  "compiler": {
    "implementation": "nulang-rust",
    "version": "0.1.0",
    "digest": "blake3:..."
  },
  "host_abi": {
    "schema": "nulang.host-effects/v0alpha1",
    "required_operations": [],
    "requires_legacy_extension_dispatch": false
  },
  "interfaces": [],
  "actors": [],
  "effects": [],
  "authority": [],
  "durability": [],
  "replay": [],
  "resources": {},
  "provenance": {}
}
```

The exact JSON schema is implementation work following this RFC; the semantic requirements below are normative for the proposal.

## Host effect ABI binding

`host_abi` is enforcement-relevant deployment metadata, not an advisory
extension. It binds the exact artifact to the compiler-owned host ABI version
and the canonical host-operation identities present in the artifact.

`required_operations` contains only canonical operation identities such as
`nulang.host-effects/v0alpha1:nulang:storage/string#Write`; deployment systems
must not reconstruct these identities from source spellings such as
`Storage.write`.

During the v0alpha1 migration, `requires_legacy_extension_dispatch` is true
when the artifact still contains an unresolved custom/legacy generic host
dispatch. A canonical-only deployment profile must reject such an artifact
rather than silently interpreting the source-level operation name.

The compiler derives this field from the same checked/lowered unit that emits
the artifact. Post-build source re-analysis is not an acceptable substitute.

## Interfaces

`interfaces` describes externally callable typed surfaces, not transport routes.

Example:

```json
{
  "name": "invoice.create",
  "input": "CreateInvoice",
  "output": "Result<Invoice, InvoiceError>"
}
```

WIT may be used as the external ABI description for Wasm Components. The Behavior Manifest references or digests the interface contract rather than replacing WIT.

## Actors

Each externally addressable or durable actor may expose:

- stable logical type/name;
- protocol/interface identity;
- durability class;
- state schema identity;
- declared supervision class where semantically relevant.

Runtime placement, shard IDs, node IDs, CIDs, process IDs, and transport addresses are explicitly excluded.

## Effects

The compiler emits the closed or bounded set of effects required by public behavior where it can be inferred.

Example:

```json
{
  "effect": "Storage.write",
  "class": "external",
  "determinism": "nondeterministic",
  "replay": "requires-journal"
}
```

Effect metadata may classify:

- `pure` / `local` / `external`;
- deterministic / nondeterministic;
- reversible-local / externally irreversible;
- replay-safe / replay-recorded / requires downstream idempotency;
- billable/advisory cost class.

These classifications must describe the semantic boundary precisely. Local snapshot rollback must never be represented as undoing a remote commit.

## External authority

The manifest serializes the program's **required authority shape**, not credentials.

Examples include:

- filesystem path classes;
- outbound network host/port constraints;
- secret namespaces;
- inference access;
- payment/financial authority;
- durable state namespaces;
- inter-actor delegation constraints.

A deployment policy may grant a strict subset of the requested authority. It must never silently grant more because the manifest omitted or failed to parse a field.

Reference capabilities (`iso`, `val`, etc.) remain a language-level memory/aliasing concept and are not serialized as external deployment credentials.

## Durability and state schemas

Durable actors/entities/workflows should expose stable schema identity sufficient for a runtime to reject unsafe deployment or migration.

Conceptual entry:

```json
{
  "owner": "PaymentProcessor",
  "schema": "payment-state/v7",
  "persistence": "durable",
  "migration_contract": "blake3:..."
}
```

The manifest need not contain raw state schema definitions if those are independently content-addressed.

## Replay and external side effects

Replay metadata exists to prevent deployment/runtime layers from over-claiming exactly-once behavior.

Conceptual classes:

- `pure`;
- `local-replay-safe`;
- `journal-result`;
- `external-idempotent`;
- `external-requires-idempotency-key`;
- `external-nonreplayable`.

For irreversible external effects, stable operation identity and downstream/provider deduplication requirements should be explicit where known.

## Resource intent

`resources` contains portable requirements/limits, not provider SKUs.

Examples:

- memory maximum/minimum;
- CPU weight/class;
- accelerator class;
- inference budget;
- deadline/latency class;
- data residency or locality labels;
- network-egress class.

Resource fields are policy inputs, not promises that the runtime will satisfy impossible requirements.

## Provenance

At minimum, provenance should be able to bind:

- source tree/content digest;
- dependency lock/graph digest;
- compiler identity/digest;
- language version;
- executable artifact digest;
- referenced interface/schema digests.

Signing/attestation is deliberately outside the compiler semantic schema. A deployment system may sign or attest the tuple `(artifact digest, behavior manifest digest, provenance inputs)` using Sigstore, KMS/HSM, workload identity, or another mechanism.

## Canonicalization and digest

The first implementation should define one canonical byte representation for hashing. Recommended approach:

1. JSON is the human-readable interchange form.
2. Canonical JSON serialization is specified for digest calculation.
3. Unknown map ordering must not change the digest.
4. The executable artifact digest is calculated independently and embedded in the manifest.
5. The manifest digest is calculated after canonicalization and used by deployment/provenance systems.

If JSON canonicalization proves awkward, a later schema version may use deterministic CBOR internally while retaining JSON tooling output.

## Compiler commands

Current experimental package-build surface:

```bash
nulang nula build-wasm
```

With the `wasm-backend` feature enabled, this command resolves the package,
checks and lowers one compilation unit, and writes:

```text
.nula/dist/<package>.wasm
.nula/dist/<package>.cwasm
.nula/dist/<package>.behavior.json
```

The Wasm bytes and Behavior Manifest are produced in the same compiler process
from the same checked/import-resolved unit. The AOT artifact is then derived
from those exact Wasm bytes. Standalone inspection/admission CLI spelling
remains non-normative until implementation review.

## Admission model

The intended deployment flow is:

```text
source
  -> type/effect/authority checking
  -> executable artifact
  -> behavior manifest
  -> artifact + manifest digest binding
  -> platform policy/admission
  -> execution
```

A deployment platform may reject an artifact because:

- requested authority exceeds organization policy;
- required effect class is forbidden;
- durable schema migration is incompatible;
- residency/resource constraints cannot be satisfied;
- artifact/manifest digests do not match;
- schema version is unsupported;
- provenance or signature policy fails.

## Compatibility

The Behavior Manifest has its own schema version and must not inherit Frozen status merely because it describes Frozen/Stable language semantics.

Before v1 of the manifest:

- fields may be renamed or restructured;
- consumers must explicitly negotiate supported schema versions;
- generated manifests must remain reproducible for a fixed compiler/source/lockfile tuple.

After a future v1 stabilization, additive advisory fields may evolve independently while enforcement-relevant semantic changes require a schema version transition.

## Security considerations

- The manifest is untrusted input until its artifact binding and provenance are verified.
- A manifest cannot grant authority; it only declares requested authority and semantics.
- Cloud/runtime policy is authoritative and may only restrict requests.
- Parser implementations must use bounded input sizes and fail closed on duplicate/ambiguous fields.
- Security decisions must use canonical parsed values, not raw strings supplied by source annotations.
- Source-generated annotations must pass ordinary type/effect/authority checking and cannot bypass compiler inference.

## Implementation sequence

### Phase 0 — RFC and schema prototype

- define `v0alpha1` JSON schema;
- add internal compiler data structures;
- emit artifact identity, package metadata, inferred effect inventory, and external authority inventory;
- add deterministic canonicalization tests;
- no Cloud enforcement.

### Phase 1 — durability/replay metadata

- emit durable actor/entity/workflow schema identities;
- emit effect replay classifications;
- add golden fixtures and backend-equivalence tests.

### Phase 2 — deployment integration

- Nulang Cloud accepts the sidecar in shadow/observe-only mode;
- verify digest binding and schema compatibility;
- compare requested authority/effects against deployment policy;
- do not widen authority from manifest content.

### Phase 3 — policy enforcement and provenance

- fail-closed admission on enforced fields;
- sign/attest artifact + manifest digest;
- bind runtime workload identity and effect/audit records back to the admitted manifest.

## Acceptance criteria for the first implementation

1. Two builds of the same source + lockfile + compiler produce byte-identical canonical manifests.
2. Manifest artifact digest matches emitted Wasm bytes.
3. A program using `Storage.write` cannot emit a manifest that omits the effect.
4. Required external authority cannot be widened or hidden by source annotations.
5. Unsupported schema versions fail explicitly.
6. Bytecode/Wasm backends emit semantically equivalent effect/authority inventories for supported common source.
7. No HIR/MIR/runtime placement identifiers leak into the semantic schema.
8. Existing programs compile unchanged when manifest emission is not requested.

## Relationship to existing RFCs

- RFC 0001 remains the executable format-stability contract.
- RFC 0002 remains the Frozen Core language contract.
- RFC 0010 provides the 100-year architecture rationale: stable semantics, replaceable backends, AI terminology decoupling.
- RFC 0019 semantic closure remains authoritative for backend-equivalent semantics, affine continuations, actor turn isolation, and typed external authority.

The Behavior Manifest does not replace those contracts; it makes selected externally relevant semantics portable and inspectable.