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
