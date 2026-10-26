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

The gate is deterministic and performs no public network calls, sleeps, or
wall-clock assertions. It covers semantic-site artifact propagation, storage
recovery, crash-window behavior, and the lower-level durable-effect contracts.

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

### HTTP operations on replayed execution paths

Ambient HTTP operations are network effects. They are prohibited in durable
workflow steps and compensations, persistent actor behaviors, and entity
replay handlers, just like Net and FS operations. Crash recovery can otherwise
repeat network operations or observe different responses. Calls to local
helper functions containing these operations are also checked.

Non-durable HTTP keeps its existing resource-capability requirements. Durable
external work requires a replay-stable, receipt-backed adapter; ordinary
HTTP is not safe merely because it appears inside a workflow.

## Replay-identified workflow output projection (experimental)

Workflow steps must **not** use ambient `perform IO.print` as a durable
output mechanism. Instead, a workflow may record the typed custom event
`emit WorkflowOutput("message")`. Runtime workflow event journaling attaches
an activation-scoped `WorkflowReplayEventId` and consumes that same event on
replay rather than appending a second copy. This is *journal data*, not stdout
or an external notification.

The host-only Rust API
`workflow_output::read_workflow_output_receipts(store, actor_id, after_sequence, max_items)`
reads a bounded, sequence-based page of already-journaled outputs, each with a
replay identity and stable `idempotency_key()`. It checks the complete retrieved
journal before returning a page, rejecting legacy output without identity,
malformed payloads, cross-actor identities, duplicate replay identities
(including collision with timer events), and out-of-order sequences. Ordinary
non-output workflow events are excluded. The API never performs network I/O,
prints messages, mutates the journal, or marks external delivery complete.

An external output consumer **must** persist and enforce deduplication of the
provided idempotency key or use a separately durable dispatch/receipt adapter.
Re-reading a page after a crash will return the same journaled outputs, but
that alone cannot guarantee single delivery: a crash between provider commit
and consumer acknowledgment can still result in repeated delivery. Do not
mistake the read-only output projection for an end-to-end exactly-once outbox.
The existing durable-effect coordinator supplies an explicit Prepared/Completed
state machine for suitably authorized and idempotent provider calls; connecting
that state machine to live workflow output delivery is a separate release gate.

### Fail-closed workflow journal reads

The workflow-output projection must distinguish an absent journal (no output
recorded yet) from a journal that could not be read in full. The new
`PersistenceStore::try_read_workflow_events` returns `io::Result` and is used
by `read_workflow_output_receipts`, which returns a `Storage` error rather than
an empty or partial page when a backend read, decoding, or JSON validation
fails. The fallible path covers the in-memory, JSON-file, libSQL, RocksDB, and
PostgreSQL stores; any other backend defaults to `Unsupported`, not an unsafe
success fallback. The legacy infallible reader remains for existing runtime
callers and is **not** suitable for new receipt/delivery integrations.

The JSON-file backend treats only a genuinely missing events file as an empty
journal; unreadable or malformed present files are errors. A single corrupt
record invalidates the entire candidate page, even if it falls after the page
cursor. Storage I/O errors must never be interpreted as proof that no durable
effect was recorded.


### Fail-closed workflow event emission (experimental)

For execution under an actor-backed runtime, `Emit` now uses a fallible
`ActorVmCallbacks::try_emit_event` callback. Workflow journal append, read and
replay-identity conflicts are propagated to the bytecode VM as execution
errors, not merely logged. Runtime staging writes the journal before
publishing a matching event to the in-memory event log, and reports required
checkpoint failures. Failed append operations do not consume the replay
ordinal or expose an uncommitted workflow output.

Compiled emit helpers return a 1/0 commit status rather than void. Native
Cranelift code checks the result and branches to an early return on failure;
the existing first-error-wins AOT error slot makes the error visible to the
AOT driver. Do not perform external work after a failed emit. The legacy void
`Runtime::emit_event` and `ActorVmCallbacks::emit_event` entry points remain
for compatibility and are **not** suitable as durable output/delivery
contracts. The non-workflow EventSourced legacy multi-field append path is
not an atomic workflow receipt transaction.

This is still **not** an acknowledged outbox. External delivery needs a
provider-idempotent consumer or a durable Prepared/Completed effect adapter
with crash-window tests. Complete exact-head CI, optional backend and
compiler profile validation, and security review before enabling production
receipt delivery.

### Receipt-backed workflow output dispatch (experimental trusted host)

`workflow_output_delivery::deliver_output_with_deduplication` connects a
**verified** `WorkflowOutput` journal receipt with the existing durable effect
coordinator. It first validates the entire workflow journal through the
fallible read API, rejecting a forged/missing/ambiguous receipt. The durable
request binds version, workflow actor, sequence, replay activation/ordinal,
exact destination and output text. A replay whose destination or payload
differs from the previously Prepared or Completed request is rejected before
any provider call.

On a new delivery, Prepared is **atomically committed** before invoking the
provider. After a successful provider response, Completed records the result.
A completed receipt is returned during replay **without redispatch**. If the
provider returns an error or response is lost after acceptance, the state
remains Prepared and recovery **must retry the same idempotency key**. The
provider MUST durably deduplicate that key: a server that merely logs keys
without enforcing them can still perform the external action twice. A trusted
host must authenticate the actor/fencing epoch, authorize the exact delivery
destination, configure retry/backoff, and use a compatible atomic persistence
backend. This is a host Rust adapter, **not** an ambient VM network effect or
an independent permission grant.

Neither this adapter nor Prepared/Completed journal records provide an
unconditional exactly-once guarantee across a remote boundary. Crash windows
between provider acceptance and client acknowledgement cannot be closed by
local journaling alone. The currently supported contract is provider-enforced
**effectively-once deduplicated delivery**. Provider receipts and request
identities must be retained for at least the workflow's retry horizon; new
provider integrations require a separate security and replay review.

### Concrete local acceptance provider (integration reference only)

`workflow_local_provider::LocalAcceptanceProvider` is a real on-disk,
key-to-result **acceptance ledger** for exercising the verified output-delivery
adapter. It atomically accepts a 64-character lowercase durable-effect key,
exact destination and bounded output bytes. Acceptance is **only the durable
local receipt**; no HTTP request, email, charge, or remote side effect occurs.
A subsequent call with the same key and request returns identical receipt
bytes, including after a fresh provider instance opens the same directory.
Using the same key for another destination or payload is rejected rather than
silently performing new work.

Records are versioned and include the destination, payload digest, operation
key and deterministic unkeyed integrity checksum (not an authentication proof). Each new record is completely written
and fsynced in a temporary file before an atomic, create-if-absent hard link
publishes its key. The directory is then synced before success is returned;
existing valid receipts are read back and the directory synced on retries.
Competing threads/processes may safely race to publish the same key on a
trusted **local filesystem implementing POSIX hard-link and fsync semantics**.
A corrupt or malformed published record fails closed. Temporary files are
cleaned on normal errors; a process killed mid-write may leave an unreferenced
`.pending-*` file requiring supervised cleanup. The reference store is not
supported on untrusted directories or remote/network filesystems with unknown
atomicity and durability behavior.

This is a concrete provider **simulator** demonstrating provider-side receipt
persistence and deduplication, not a deployable webhook/payment service. Its
side effect and idempotency record are deliberately the *same atomic object*;
a production provider performing a separate external action needs an actual
transactionally idempotent endpoint, provider-side retention and timeout
recovery, authorization checks, and cross-node/process failure testing. The
underlying workflow adapter must continue refusing unauthenticated destinations
and replay-unsafe ambient network effects.
