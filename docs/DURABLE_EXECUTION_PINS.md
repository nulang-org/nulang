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

These APIs are currently an **opt-in native-workflow path**, not blanket
enforcement for every workflow. An already registered bytecode recovery module
is rejected by the pinned API, because digest verification of separate bytes
does not establish that the executable module actually dispatched was loaded
from them. A verified immutable bytecode-module loader is required first.

```bash
cargo test --locked --test pinned_workflow_runtime
cargo test --locked --features sqlite --test pinned_workflow_runtime
```

The sqlite tests include subprocess-backed workflow creation, changed-artifact
rejection, and verified restart on a fresh Runtime.

### Remaining safety constraints

- The generic atomic persistence contract treats identical committed
  transitions as idempotent retries. A preflight existing-history check
  prevents ordinary repeated starts, but it is not a cross-process atomic
  **create-only** operation. A storage-level conditional creation primitive
  is still required for concurrent identical starters.
- Legacy unpinned workflow creation still uses its original separate writes.
  There is no automatic provenance check for old unpinned histories.
- Persisted event readers that silently omit every corrupt start record can
  defeat the atomic-start fingerprint. Strong recovery requires checked,
  corruption-reporting history reads.
- No immutable artifact retention/garbage-collection controller, signed
  provenance attestation, cross-node epoch test, or automatic Cloud admission
  integration has been implemented.
- Promotion requires exact-head Rust build, Clippy, all relevant feature tests,
  crash/fault-injection coverage and independently reviewed concurrency tests.

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
