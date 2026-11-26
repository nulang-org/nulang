# Durable Execution Guarantees

This document defines the claim boundary for Nulang durable external effects.
It is intentionally narrower than "exactly once".

The executable contract spans:

- `src/semantic_identity.rs` — compiler-owned static effect-site identity;
- `src/durable_effect.rs` — effect state machine and delivery semantics;
- `src/durable_effect_runtime.rs` — storage-neutral prepare/recover/complete coordinator;
- `src/runtime/persistence.rs` — atomic transitions, activation fencing, and recovery lookup;
- `src/durable_effect_persistence.rs` — versioned persisted effect records.

The end-to-end deterministic release gate is
`tests/durable_effect_failure_matrix.rs`.

## What Nulang can guarantee locally

For one logical durable external effect, the runtime contract is:

1. derive the static call site from compiler-owned semantic identity;
2. combine it with replay-stable execution identity to derive one `DurableEffectId`;
3. persist a `Prepared` record atomically before provider execution;
4. bind the record to the exact request digest and full effect specification;
5. reuse the same operation ID on every recovery attempt;
6. persist the first terminal result as `Completed`;
7. replay a completed result without redispatching the provider;
8. reject request or specification drift before dispatch;
9. reject stale activation epochs at the persistence boundary;
10. model compensation as another explicit durable effect rather than pretending
    the original external mutation can be rolled back.

A Nulang-local journal or transaction cannot prove that an arbitrary remote
system executed an operation exactly once. A process can always fail after the
remote side commits and before Nulang records the receipt.

## Crash-window matrix

| Crash point | Durable state after restart | Required recovery | External observation |
|---|---|---|---|
| Before intent commit | No effect record | Start a logical attempt only if the owning durable turn was not committed | No provider call should have occurred |
| After intent, before provider | `Prepared` | Retry the same semantic operation ID | One call if the first process never reached the provider |
| After provider commit, before receipt | `Prepared` | Retry with the same operation ID | May duplicate under at-least-once; effectively-once requires provider/backend deduplication |
| After receipt, before actor/workflow resume | `Completed` | Return the recorded result | Provider must not be called again |
| Replay request/spec differs | `Prepared` or `Completed` | Fail closed | Provider must not be called |
| Backend owns semantics | Backend-defined | Delegate to the backend contract | Nulang must not strengthen the guarantee |

## Delivery semantics

### `AtLeastOnce`

Recovery may execute the dependency again. Duplicate external mutation is
permitted by the contract. Callers must use an idempotent operation, explicit
deduplication, or accept duplicates.

### `EffectivelyOnceWithDeduplication`

Nulang retries with the exact same stable operation ID. The external dependency
must actually honor that key (or provide an equivalent query/dedup mechanism).
Nulang can then observe one logical outcome across retries, but this is not a
blanket exactly-once guarantee for an uncooperative provider.

### `BackendDefined`

The configured backend owns retry, deduplication, and commit semantics. Nulang
delegates rather than inferring a stronger guarantee.

## Compiled async-effect identity bridge (experimental)

`DurableEffectSpec::from_compiled_async_site` accepts a `CodeModule`, exact
`PerformAsync` program counter, stable actor/turn identity, and dynamic site
occurrence. It rejects missing site metadata, a non-`PerformAsync` opcode, a
constant-pool operation mismatch, and a missing execution key. It derives the
same logical `DurableEffectId` as compiler-owned semantic-site identity rather
than inventing another effect identifier in every provider integration. Callers
must first admit a verified/pinned compiled artifact: checking an opcode and
constant does not establish semantic-ID provenance from untrusted metadata.

`tests/effect_site_artifact_metadata.rs` now checks this bridge against a
compiled `.nula` actor behavior, an NBC encode/decode round trip, formatting
changes, a simulated provider commit before local receipt, repeat dispatch with
the same idempotency key, and replay/mismatch behavior in `MemoryStore`.

**This is not a .nula end-to-end durability guarantee.** The test invokes the
coordinator directly from Rust; the production bytecode effect callback is
not yet forced through this coordinator. `from_compiled_async_site` only
constructs a validated specification: it does not persist a `Prepared` intent,
dispatch the provider, or enforce provider cooperation. Full acceptance still
requires a VM/host dispatch adapter that journals *before* the external call,
tracks replay-stable turn keys and invocation occurrence across process restarts,
integrates terminal receipts atomically with the owning turn, and injects real
restart/failover faults. This work must preserve source-level effect authority
checks and never reinterpret `DeliverySemantics` as a stronger guarantee than
the external provider supplies.

## Compiled external-effect dispatch helper (experimental)

`DurableEffectCoordinator::execute_compiled_async_with_deduplication`
takes the live owning `Actor`, an exact typed `AuthorityGrant`, a verified
compiled module, exact effect-site PC, replay-stable execution key and occurrence,
request bytes, and a provider callback. It rejects a mismatched actor ID, terminated actor, or
activation epoch, requires the complete current actor grant manifest to parse,
and verifies that an unparameterized `Other(Namespace::Operation)` grant
matches the compiler's `Namespace.operation` effect site. All of these checks
happen before journal reads, `Prepared` writes, or provider dispatch.

**Fail-closed restriction:** This generic bridge does not authorize
resource-parameterized or built-in grants (`Net::TcpOut`, `Fs::Read`,
`Secret::Read`, etc.). Those require specialized host adapters that bind the
exact evaluated destination/resource to authority. The host must supply a
trusted live actor and correctly matched provider; caller-controlled actor
objects, forged artifacts, or a provider closure that invokes a different
operation are outside this proof.

Once authorized, the coordinator obtains one canonical effect ID and commits
`Prepared` **before** calling the provider,
supplies the stable idempotency key, persists a terminal receipt, and replays
a completed result without redispatch. Provider errors leave `Prepared`
intact because the external operation may already have committed.

When a prior effect is still `Prepared`, the coordinator reads its committed
activation tail and refuses redispatch unless the caller's epoch matches the
durably claimed epoch. A new owner must claim its epoch before retrying.
A completed receipt remains available read-only across epoch changes.
This is a **read fence**, not a lease or a proof that an owner cannot change
between the epoch check and external dispatch. The external provider must
actually deduplicate the idempotency key.

`tests/authorized_compiled_durable_dispatch.rs` also covers missing
or malformed grants, wrong or terminated actor, wrong activation epoch, unrelated/parameterized
authority, revocation before retry or read-only receipt replay, and valid
grant/receipt reuse. These cases assert no journal mutation on denial.

`tests/compiled_durable_dispatch.rs` exercises source-compiled effect
metadata through the coordinator, timeout recovery, stable provider
deduplication and replay, drift rejection, and stale-epoch rejection using
`MemoryStore`. This is a Rust integration path; it is **not** proof that
the VM's ordinary `.nula` `PerformAsync` host callback uses the helper
or that process restarts, multi-owner race conditions, all resource-scoped
external authority, and transactional actor-turn completion have been validated.

`tests/durable_compiled_store_restart.rs` extends the trusted Rust adapter
checks to file-backed SQLite: it compiles and round-trips an NBC artifact,
commits and reopens a terminal receipt, checks request drift and revoked
authority after reopen, and verifies that a `Prepared` timeout preserves a
stable provider idempotency key across a fresh store instance. The gate
explicitly runs this regression with the `sqlite` feature. It does **not**
execute the VM's live `PerformAsync` callback, kill a process, verify provider
side-effect deduplication, or prove atomic actor-turn completion. Those require
separate integration and crash tests.

## OS-process crash acceptance on file-backed SQLite

`tests/durable_effect_process_kill.rs` uses the same Rust trusted-host coordinator
and a file-backed provider test double that deduplicates the logical operation
ID. A parent waits for an atomically published checkpoint marker, forcibly kills
the actual child process, and starts a fresh process against the same database.
On Unix it verifies the process exited due to SIGKILL. Recovery subprocesses
have bounded timeouts and are reaped on assertion failures.

The test covers three acknowledged checkpoints: after intent commit, after
provider mutation but before terminal receipt, and after terminal receipt but
before resumption. It also covers an epoch-2 ownership claim following a crash,
rejecting stale epoch-1 retry and receipt commit. Every scenario then does one
more independent restart, checking unchanged journal sequence, stable idempotency
keys, one provider mutation and no redispatch after a terminal receipt.

Run the focused test with:

```bash
cargo test --locked --no-default-features --features sqlite --test durable_effect_process_kill
```

This is not a power-loss or mid-SQLite-commit test. The provider is a cooperative
local test double; the fixture does not qualify arbitrary providers, concurrent
live owners, the normal VM callback, or atomic actor-turn completion.

## Opt-in VM asynchronous effect bridge (experimental)

The VM exposes a backwards-compatible asynchronous callback with the exact
`PerformAsync` instruction location and owning code module.
`DurableEffectCoordinator::execute_vm_async_text_with_deduplication` is an
explicit trusted-host adapter to the existing authorized compiled-effect
coordinator. The caller must provide a trusted live actor, verified artifact,
correct grant and provider, canonical request bytes, and a replay-stable
execution key and occurrence. The adapter validates the compiled effect site
and authority before journaling or dispatch. It supports text receipts only:
invalid UTF-8 fails closed, including after reopening a completed journal,
without redispatching the provider.

`tests/durable_vm_callback_bridge.rs` executes actual VM bytecode against
file-backed SQLite. A subprocess pauses after atomically publishing a
checkpoint following a provider ledger write and fsync, before acknowledging
the receipt; its parent kills and reaps it (SIGKILL on Unix). A fresh VM and
store in the surviving parent recover using the same provider idempotency key,
and completed replay does not contact the provider. The fixture also exercises
authority, request drift, and terminal invalid text receipt recovery.

```bash
cargo test --locked --no-default-features --features sqlite --test durable_vm_callback_bridge
```

This is an opt-in trusted test adapter, not the default actor runtime.
`RuntimeVmCallbacks` and `BytecodeRuntimeCallbacks` remain unchanged.
The test does not establish artifact provenance, persistence of execution
keys and occurrences across arbitrary actor-turn replay, resource-scoped
authority, atomic actor-turn/inbox completion, real provider cooperation or
multi-owner safety. Default runtime integration remains gated on these
requirements.

### Activation-scoped workflow occurrences (experimental)

The opt-in `execute_workflow_vm_async_text_with_deduplication` adapter
derives the execution key from the live actor's `WorkflowActivationId`:
`workflow-command:v1:<accepted-command-sequence>`. The effect ID additionally
includes the actor identity, compiler-owned semantic site, and a deterministic
activation-local occurrence ordinal. The ownership epoch is a fence, not part
of the logical effect ID.

A lazily allocated actor-owned cursor tracks the next occurrence separately
from custom workflow-event replay. It advances only after a terminal text
receipt is successfully returned, including receipt replay. A failed attempt
keeps the current occurrence and pins the site/request while the actor lives.
Retry with a different site or request, invalid/mismatched activation, an
older command, or a transferred cursor fails closed. Storage-proven open
admission with the same owner epoch is required even for receipt replay.

`DurableTransition::workflow_command` constructs an explicit standalone command
admission. MemoryStore and SQLite persist the command journal entry and current
open-command metadata together with the fenced tail. They require a safe
pre-command snapshot and reject overlapping admissions, stale owners, legacy
writes while open, unbound effects, and closure while an effect is Prepared.
The native synchronous workflow admission helper uses this contract; the
opt-in compiled VM adapter queries the same metadata before dispatch and replay.
A generic command or old atomic tail without the explicit marker is not proof
of admission; it is not backfilled or reinterpreted. The new marker participates
in the transition digest, and absent markers preserve existing v1 digests.
SQLite's additive `durable_workflow_admissions` table records the latest
admission and terminal sequence. Old code does not enforce this metadata;
mixed-version writers, live upgrades and rollback are not qualified.

The trusted host must still use the same verified/pinned bytecode and replay
from the safe pre-command boundary. The ordinal is reconstructed by deterministic
execution of the recovered command, not serialized as a continuation or inferred by
counting journal records. The new version-2 workflow-effect envelope binds the
activation and occurrence to the semantic site and canonical effect ID in the
same atomic transition as Prepared; Completed retains that binding and request
digest. The MemoryStore and SQLite implementations find the slot without the
caller guessing its site and check the binding and monotonic state inside the
commit boundary. Gaps, duplicate new transitions, completion without an intent,
and changed site/request/receipt fail closed. Generic v1 effect and compensation
records retain their previous encoding. An unbound effect in the active command
window is ambiguous and is refused rather than automatically migrated. Older
readers do not support the v2 envelope; reverting after a v2 write requires a
compatible recovery path, such as restoring a consistent pre-v2 backup. Live
upgrade and rollback are not qualified.

This experimental path scans the post-command portion of effect history; there
is no dedicated index or throughput claim yet. Backends without slot recovery
return Unsupported. While an explicit terminal transition stores the snapshot
and closes admission in one commit, it does not prove deterministic execution
consumed all previously completed effects. Terminal cursor reconciliation,
inbox/outbox integration, atomic intermediate events, timers and suspension,
and production recovery with a new ownership epoch remain future work.
The general compiled workflow scheduler still uses the legacy journal path
and **must not** use this adapter until those paths are qualified.

The existing SQLite VM fixture exercises this opt-in path with compiled
workflow bytecode, a committed command sequence, lost provider reply,
repeated dynamic execution of one site, and replay from a reopened store
with a fresh actor. Distinct occurrences and new commands produce distinct
keys, while replay of completed occurrences does not call the provider.
The later-occurrence case also checks a newer ownership epoch, pending site
drift, and reuse of the original logical key after replay of an earlier receipt.
A reopened-store regression rejects site drift with an empty in-memory cursor
both before and after completion and replays the original receipt without a
provider call. Storage tests exercise immutable bindings, monotonic completion,
idempotent exact-tail retry and rollback of conflicting writes in both MemoryStore
and SQLite.
The admission tests also distinguish a generic atomic tail from true workflow
admission, deny a closed command from a fresh VM cursor after database reopen,
and inject failures in both SQLite admission and closure writes to verify
atomic rollback. These are trusted fixture results, not proof of production
workflow recovery.

## What is not yet a production-wide guarantee

Passing this gate does **not** prove that every `perform`, workflow activity,
LLM call, HTTP request, or actor turn is wired through the durable coordinator.
Each adapter is production-ready only after its dispatch path consumes the
compiler-produced effect-site metadata and uses the same persisted operation ID.

A production external-effect adapter should have all of the following:

- compiler/runtime-stable semantic effect-site identity;
- validated bytecode/NBC effect-site metadata;
- intent persistence before dispatch;
- request and specification validation on recovery;
- a stable provider idempotency/deduplication key where required;
- a terminal receipt that replays without redispatch;
- activation fencing so stale owners cannot publish late results;
- atomic integration with the owning actor/workflow turn where its result
  affects durable state or inbox deduplication;
- trace metadata containing effect boundary, delivery semantics, and stable
  operation identity;
- deterministic crash-window tests;
- provider-specific timeout, throttling, response-loss, and failover tests.

## Release gate

Run:

```bash
bash scripts/test-durability-guarantees.sh
```

The gate performs no public network calls. It covers semantic-site artifact
propagation, storage recovery, deterministic crash-window behavior and real
OS-process termination/recovery on SQLite, including the opt-in VM callback. Process waits are bounded by the
fixture timeout; logical correctness is asserted from the durable journal and
provider ledger, not elapsed wall-clock time.

## Competitive benchmark contract

Correctness comes before throughput. Once the same production adapter path uses
this contract, benchmark Nulang against durable-workflow systems with an
equivalent workload:

1. start a durable job;
2. perform a provider mutation with an idempotency key;
3. kill the process after provider commit but before local receipt commit;
4. restart on a fresh process;
5. complete the job;
6. assert external mutation count, recovered result, time to recovery, and
   steady-state overhead.

Report semantics separately from speed. A system that completes faster by
allowing duplicate mutation is not equivalent to one that closes the crash
window through deduplication.
