# Durable Execution Artifact Pins (v1)

**Status:** Experimental, runtime-neutral recovery admission primitive. Not yet wired
into automatic workflow creation/recovery or Nulang Cloud deployment admission.

## Purpose

For a *specific* durable execution, structural schema compatibility does not
prove that new executable code will reproduce the original work. In particular,
`BehaviorManifest::validate_upgrade_from` checks durable state shape and
migration topology; it is **not** a historical replay certificate.

`DurableExecutionPin` enforces the narrower safe contract: recovery is admitted
only with exactly the originally pinned executable bytes and the same complete
behavior manifest. Unknown pin versions, absent metadata, invalid sidecars,
different execution identities, or changed code all fail closed.

This contract is independent of Temporal wire compatibility and does not
require a new serialized `.nbc` format.

## Pin contents

The versioned pin records:

- exact stable execution ID (never a "latest run" alias);
- package identity;
- compiler-derived artifact ID;
- BLAKE3 digest of the executable bytes;
- domain-separated BLAKE3 digest of the canonical behavior manifest.

`BehaviorManifest::verify_artifact_bytes` checks the actual supplied binary
before a pin can be created or validated. Changing any manifest field that
contributes to its canonical digest rejects resume even if the .nbc bytes
remain unchanged.

## Manual preflight

Build and generate a pin at the execution creation boundary:

```bash
cargo run --locked --bin nulang-execution-preflight -- pin \
  payments/run-42 payments.behavior.json payments.nbc > run-42.pin.json
```

Check the exact pinned execution and the executable chosen for recovery:

```bash
cargo run --locked --bin nulang-execution-preflight -- verify \
  payments/run-42 run-42.pin.json payments.behavior.json payments.nbc
```

The command returns nonzero for missing/corrupt metadata, an invalid sidecar,
an unknown pin version, a different execution ID, or incompatible bytes/manifest.

**Important:** Redirecting CLI output to a file is *not* an atomic execution
creation protocol. A production runtime or host must commit the pin and initial
execution record in one durable transaction, load the pin before replay, and
retain the corresponding immutable artifact until the execution is no longer
recoverable. Do not bypass an absent pin by assuming the latest deployment is
safe.

Pins are not cryptographic signatures. Protect their storage/transport integrity
at the same trust boundary as execution history, and verify artifact provenance
separately if the artifact store is untrusted.

## Verification

```bash
cargo test --locked --test durable_execution_pin
cargo test --locked --bin nulang-execution-preflight
bash scripts/test-durability-guarantees.sh
```

The focused tests cover exact-match recovery, different executable bytes despite
structural schema compatibility, changed manifest identity with unchanged bytes,
incorrect executable/sidecar binding, cross-execution reuse, unknown pin version,
missing pin, and malformed digests.

## Atomic storage integration (stacked increment)

`runtime::commit_pinned_workflow_start` creates a pinned workflow's initial
`ActorSnapshot`, `WorkflowStarted`, and reserved execution-pin record in
one `DurableTransition` at sequence 1. This uses the existing
`PersistenceStore::commit_transition` transaction boundary and refuses to
initialize actor histories that already have a committed predecessor.

`runtime::verify_pinned_workflow_resume` checks the atomic tail, snapshot,
unique initial start record and pin, then verifies the provided executable
bytes and complete manifest before the host rehydrates any actor.

The pin is encoded as a reserved `WorkflowEvent::Custom` record inside the
**same initial atomic transition**. It is not an ordinary user-visible workflow
command. The implementation deliberately does not introduce new tables, a
new bytecode format, or a second journaling backend.

Example host integration:

```text
# Creation path — before publishing or scheduling any actor:
snapshot = stage_initial_workflow_snapshot(actor)
commit_pinned_workflow_start(store, actor_id, execution_id,
                             name, manifest, executable_bytes,
                             snapshot, initial_state)
publish_actor_only_after_commit()

# Restart path — before resolving mutable 'latest' code or running handlers:
immutable_bytes = artifact_store.resolve(pinned_artifact_digest)
verify_pinned_workflow_resume(store, actor_id, execution_id,
                              pinned_manifest, immutable_bytes)
recover_actor_using_verified_module()
```

The Rust APIs implement the two persistence-side operations above, **not**
automatic `Runtime::try_spawn_workflow_actor` / `Runtime::recover_actor`
interposition. An embedding host must enforce that sequencing and must not
fall back to a legacy recovery API after a failed preflight. In particular,
an executable loaded from an unpinned mutable "latest" alias is not acceptable.

`MemoryStore` and `LibsqlStore` are covered by deterministic regression
tests, including an integration test that exits the first test process, opens
libSQL in another process, verifies the original artifact, then rejects a
changed candidate. Backends without atomic `commit_transition` support
must return `Unsupported` and leave no partially created history.

Focused verification:

```bash
cargo test --locked --test pinned_workflow_atomic_start
cargo test --locked --features sqlite --test pinned_workflow_atomic_start
cargo fmt --all -- --check
```

**Remaining release blockers:** bind these two APIs to the automatic actor
spawn and recovery paths; register/resolve an immutable artifact before
activation; add fail-closed host-level migration policy, stale-owner fencing,
transaction rollback/fault-injection, and end-to-end crash-point coverage.
Do not advertise transparent worker upgrades until that work is complete.

## Opt-in Runtime lifecycle integration

`Runtime::try_spawn_pinned_workflow_actor` now uses the atomic creation
primitive to stage initial actor state and commit the snapshot, start event,
and pin **before** scheduling the actor. A failed atomic write removes the
unpublished actor from memory. It intentionally does not clear the durable
store on an ambiguous commit error: recovery must first inspect the committed
history. Duplicate starts on an already-persisted actor ID are rejected by a
preflight history check.

`Runtime::recover_pinned_workflow_actor` reads the durable pin, calls a
host-supplied `ImmutablePinnedArtifactResolver` **by the original executable
digest**, verifies the bundle against the persisted execution identity and
complete behavior manifest, and only then hydrates the actor.

Plain `Runtime::recover_actor` rejects an execution pin or the recognizable
atomic-start fingerprint. This prevents a legacy bypass when a decoded pin
is missing from otherwise recognizable pinned start history.

These APIs remain **opt-in**. Schema-backed workflow recovery now installs
a verified decoded NBC module, but it still rejects a separately registered
module. Automatic compiler-backed pinned creation and Cloud admission remain
separate work.

```bash
cargo test --locked --test pinned_workflow_runtime
cargo test --locked --features sqlite --test pinned_workflow_runtime
```

The sqlite tests include subprocess-backed workflow creation, changed-artifact
rejection, and verified restart on a fresh Runtime.

### Remaining safety constraints

- Pinned first transitions are create-only in the Memory and libSQL atomic
  backends. Other transition retries continue to be idempotent. Concurrent
  creation on unsupported persistence engines is not available.
- Legacy unpinned workflow creation still uses its original separate writes.
  There is no automatic provenance check for old unpinned histories.
- Pinned resume uses checked history reads in Memory and libSQL. This
  detects unreadable/corrupt rows but not coherent malicious deletion of all
  start records; authenticated history integrity remains a future requirement.
- No immutable artifact retention/garbage-collection controller, signed
  provenance attestation, cross-node epoch test, or automatic Cloud admission
  integration has been implemented.
- Promotion requires exact-head Rust build, Clippy, all relevant feature tests,
  crash/fault-injection coverage and independently reviewed concurrency tests.

## Create-only storage and strict history validation

The pinned initial-start marker is reserved in the persistence contract as
`PINNED_WORKFLOW_START_EVENT`. A first transition containing this marker is
**create-only**: in `MemoryStore` and `LibsqlStore`, replaying the
same exact first transition returns `AlreadyExists` instead of reporting
an idempotent success. For local SQLite, this decision happens **inside**
the `IMMEDIATE` database write transaction after reading the current durable
tail, so independent concurrent starters cannot both acknowledge the same
execution. Remote libSQL contention still requires backend-specific testing. All other transition retries retain their existing
idempotent semantics. Backends without atomic transition support remain
unsupported for pinned creation.

`PersistenceStore::read_workflow_events_checked` is a separate, fallible
history read. `MemoryStore` returns the complete in-memory history and
`LibsqlStore` reports query, row-decoding, and JSON-corruption errors
rather than silently omitting the offending event. Pinned resume and the
legacy recovery guard use strict reads where supported. An unreadable row
is **not** equivalent to no recorded pin.

Relevant focused tests:

```bash
cargo test --locked --test pinned_workflow_atomic_start
cargo test --locked --test pinned_workflow_runtime
cargo test --locked --features sqlite --test pinned_workflow_atomic_start
cargo test --locked --features sqlite --test pinned_workflow_runtime
```

**Remaining limits:** a valid-looking but maliciously edited/deleted history
can evade simple row-decode checks if no independently authenticated history
commitments are verified. The current transition digest does not prove that
every expected event is still physically present. An immutable authenticated
history/transition chain and complete trusted-history scan should precede
production claims about tamper resistance. Bytecode-backed pinned execution
also still requires a verified immutable module loader. Cloud artifact retention
and deployment admission remain separate work.

## Verified immutable bytecode loader (stacked increment)

Pinned **schema-backed bytecode workflow recovery** now resolves the originally
recorded executable digest, checks the complete behavior manifest, and decodes
the **actual verified `.nbc` bytes** with `CodeModule::from_nbc`. The decoded
module is not supplied by the caller as a pre-existing in-memory
`CodeModule`: recovery validates the persisted actor schema, checks that it
is a durable workflow in both the compiler module and sidecar, requires
the compiled and declared schema versions to match, and rejects missing,
duplicate or out-of-range behavior references/bytecode offsets.

The resulting verified module, step offsets, and compensation offsets are
installed into `Runtime::recovery_modules` **before** actor hydration and
scheduling. Any independently registered recovery module for the same actor
continues to be rejected rather than silently reused. A failed hydration
cleans up the newly registered module instead of leaving unverified fallback
state. Native workflows without compiler-owned snapshot schemas retain the
existing pinned native workflow path.

This makes one narrow guarantee: **the recovered bytecode module comes from
the exact bytes checked against the durable execution pin**, not a parallel,
mutable module registration.

```bash
cargo test --locked --test pinned_bytecode_loader
cargo test --locked --features sqlite --test pinned_bytecode_loader
```

The tests cover exact verified module dispatch metadata, substitution with
another valid NBC, malformed NBC matching its own sidecar, persisted schema
mismatch, out-of-range behavior indexes, pre-registered unverified modules,
and separate-process SQLite restart and substitute rejection.

**Remaining:** the verified loader is an opt-in runtime path; it
does not automatically pin every workflow started by the general VM spawn
callback. Artifact retention, cryptographic provenance, authenticated
history verification, cross-node placement/activation fencing, and Cloud
deployment policy are also outside this slice.
The pin and event history must be read under a coherent fenced recovery
boundary in a later increment to rule out concurrent modification between
verification and hydration.

## Compiler-aware pinned workflow creation

`Runtime::try_spawn_pinned_compiled_workflow(execution_id, schema_name,
manifest, executable)` now provides an opt-in start path that uses a single
verified NBC artifact as the source of both executable code and compiler
state metadata. It performs the manifest/executable identity check, decodes
and validates the exact `CodeModule`, confirms the durable workflow schema,
and checks state defaults **before allocating an actor ID**.

Creation uses the existing pinned actor lifecycle, but stages compiler
defaults, actor-owned strings, bytecode handler offsets, compensation offsets,
and compiler-owned schema identity **before the initial snapshot**.
`WorkflowStarted`, the snapshot, and the immutable execution pin are
committed together using the existing atomic transition contract. The
module's recovery registration and scheduler enqueue happen only after a
successful durable commit. An unsupported backend or failed validation
leaves no published actor; ambiguous commit failures do not erase
potentially committed history.

The API intentionally accepts **no separate unverified `CodeModule`** and
does not infer executable identity from a mutable module registry. The
general `Runtime::spawn_from_module` compatibility path is unchanged.

```bash
cargo test --locked --test pinned_compiled_workflow_start
cargo test --locked --features sqlite --test pinned_compiled_workflow_start
```

Tests check durable compiler defaults (including heap-backed strings),
schema identity and dispatch offsets, malformed NBC, mismatched sidecars,
out-of-range behavior indexes, duplicate/unsupported compiler defaults,
unsupported storage, and database-backed restart with the same pinned
artifact.

**Important limit:** a newly created actor is staged in the private
`Runtime::actors` map while the synchronous initial transaction executes;
it is not enqueued for scheduling or returned to its caller until commit.
An external embedding host must not expose the mutable runtime midway
through this call. Cross-process starter fencing and coherent verification
and recovery snapshots still need separate crash/concurrency validation.

## Atomic recovery claim and activation epoch fencing

`Runtime::recover_pinned_workflow_actor` now observes the committed
`DurableTailPosition` **before** invoking an external immutable-artifact
resolver. After identity, bytes, manifest and compiled schema validation, it
calls `claim_pinned_workflow_recovery` against that exact observed
predecessor. The claim is a single `DurableTransition` that atomically:

- Advances the durable sequence and activation epoch together
- Preserves the fully committed snapshot state at the new epoch
- Appends a reserved `__nulang.recovery_claim/v1` workflow event
- Uses the backend's existing atomic predecessor/epoch validation

A duplicate claim against the same tail must not acknowledge two winners:
both MemoryStore and libSQL treat a claim marker as **create-only** even
when competing payloads are byte-for-byte equal. Successful hydration then
checks that the committed epoch/sequence still match the claim; failure
discards the recovered actor, module registration, and CRDT-field mapping.

The first increment **deliberately fails closed for unfinished durable
commands**: snapshot sequence/epoch must exactly match the observed atomic
tail. A tail beyond the last safe snapshot needs a separate durable
activation-replay handoff that preserves the pending command, not a blind
new-epoch checkpoint. Post-tail workflow events also block this claim.

```bash
cargo test --locked --test pinned_recovery_fence
cargo test --locked --features sqlite --test pinned_recovery_fence
```

The tests exercise competing local libSQL workers, immutable-artifact
resolver races, same-tail replay rejection, stale-epoch durable write
rejection, sequential epoch advancement, and refusal of incomplete
checkpoints.

**Safety boundary:** this is a fencing **claim**, not a renewable distributed
lease. A claimant obtaining a later epoch will fence the old actor's
subsequent atomic persistence writes, but unguarded external effects, queued
timers, and non-transactional legacy writes are not automatically fenced.
A post-hydration epoch check narrows—but does not eliminate—the interval
before an old actor becomes stale. Read-only verification and the claim are
not yet an authenticated multi-table history snapshot; corruption-resistant
event commitments and checked journal/snapshot reads remain required.
Production multi-host deployment needs owner identity, lease/liveness,
effect/outbox fencing, and fault-injected failover tests.

## Next increment / non-goals of v1

1. Atomically persist a pin at execution creation with the existing
   `DurableTransition` or equivalent host transaction, and refuse recovery
   when the pin is absent or mismatched.
2. Make the host resolve pinned executable bytes from immutable artifact storage;
   add multi-process crash/restart and stale-owner fencing tests.
3. Add *separate* historical replay command comparison and explicit migration
   authorization for changing an execution's artifact. Never treat an unchanged
   state schema or a manually asserted flag as proof of replay safety.
4. Integrate the preflight result into Nulang Cloud's canonical hosted
   deployment/admission control plane (in the separate Cloud repository).

This first version intentionally **does not** offer a replay-compatibility
certificate, automatically migrate running workflows, change Cloud placement,
or claim production-safe worker versioning.
