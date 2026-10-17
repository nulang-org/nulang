# Durable job: checkpoint, SIGKILL, recover, complete

**Status:** Executable reference application and CI gate; Nulang is alpha.
This is a *single persistent actor state-machine* demonstration on the **native reference bytecode runtime**, **not** proof that arbitrary Nulang workflows, external calls, timers, or distributed execution have production recovery guarantees.

## Prerequisites

- Linux or macOS with POSIX `SIGKILL` and standard input pipes.
- Python 3.10+ (stdlib only) to run the verification harness and SQLite baseline.
- A full-featured `nulang` CLI binary built for your platform (for example, `target/release/nulang`). The browser playground and WASM backend do not execute this demo.
- The checkpoint-gate implementation blocks on standard input with `IO.read` after the durable state is read back. No filesystem or OS capability grant is needed for the application.

From the repository root:

~~~bash
python3 -m unittest discover -s examples/durable_job -p 'test_*.py'
python3 examples/durable_job/run_demo.py --baseline-only
./target/release/nulang --check examples/durable_job/job.nula
python3 examples/durable_job/run_demo.py --nulang-bin ./target/release/nulang
~~~

The harness creates separate temporary persistence directories for Nulang and SQLite. It makes no HTTP requests and never touches `.nulang/store/` or application data in the checkout.

## Contract under test

One job starts with `(stage=0, completions=0)`. Both implementations use the same three-process experiment:

| Process | Action | Assertion |
| --- | --- | --- |
| A | Persist `stage=1`, acknowledge, then block on an open stdin pipe; harness sends **SIGKILL** | `PREPARED=1:0` observed and process actually terminated by signal |
| B | Start a **fresh** process with the same store and actor definition; complete once | `RECOVERED=1:0`, then `COMPLETED=2:1` |
| C | Start a second fresh process with the same store; request completion again | `RECOVERED=2:1`, then `COMPLETED=2:1` |

The assertion on the *read-back after a new process* is essential: merely printing a checkpoint marker in process A does not prove the state was durable. An intentionally broken mock runner that forgets state is rejected by `test_runner.py`.

The open stdin pipe is a deterministic **failure gate**, not a durable timer or a workload communication mechanism. The test kills the process after a successful actor request and read-back. It does **not** crash during a storage commit, simultaneous writers, or an external side effect.

### Nulang implementation

[`job.nula`](job.nula) defines a `persistent actor DemoJob` with two `state durable` integer fields. The state machine rejects duplicate transitions through explicit `if` checks. The CLI is invoked with `--backend bytecode --store json:<tempdir>`. The first process selects `prepare` when recovered `stage == 0`; subsequent runs select `resume` using persisted stage. This avoids unhandled `Env.get`, `System.arg`, and `FS.read` effects on the current actor-backed VM.

**Known limitation:** Nulang's present CLI restart mechanism recreates process-local numeric actor IDs deterministically; this demonstration intentionally spawns one actor in the same order each run. This is not a general stable business-key/virtual-actor identity guarantee. Do not extend it to concurrent runs sharing the JSON directory or to renamed/reordered actor spawns without new migration and ownership tests.

### Conventional baseline

[`sqlite_baseline.py`](sqlite_baseline.py) implements the **same state transitions** in Python with stdlib SQLite and explicitly written SQL transactions, conditional updates, and a persistent database file.

Both programs run with one OS process at a time, one local storage backend, and no external network dependencies. SQLite provides its normal transactional semantics; this is not a performance or operational benchmark of SQLite against Nulang's JSON-file store.

## Comparison: what code each approach owns

| Concern | Nulang | Python + SQLite |
| --- | --- | --- |
| Application data model | Actor-owned durable fields | Explicit SQL table |
| State transition | Actor behavior, guarded by stage | Transaction + conditional UPDATE |
| State persistence | Native runtime `--store json:<dir>` and snapshot/journal | SQLite transaction commit |
| Process restart | Re-spawn actor with same process-local ID and store | Reconnect database, select row |
| Duplicate completion | Application-level stage guard | Application-level WHERE predicate |
| Workflow timeouts, external effects, distributed failover | **Not tested** | **Not tested** |
| Provider idempotency | **Not implemented in this demo** | **Not implemented in this demo** |

This comparison evaluates *source-level plumbing*, not a claim that Nulang removes operational costs. The runtime and persistence layer are still infrastructure; their upgrade, backup, recovery, schema evolution, and production failure boundaries must be considered separately.

For a Temporal comparison, a fair later study must implement the same externally idempotent activity, its retry/receipt model, durable timers, worker process recovery, and provider failure injection in both systems. Comparing this toy state machine to a complete Temporal service would not establish superiority.

## What counts as success and failure

The driver exits nonzero if the Nulang binary is absent, the interpreter exits before the checkpoint, the `SIGKILL` is not confirmed, the recovery stage differs, or a duplicate completion changes the counter. It also rejects timeout and runtime errors. A green **Python baseline-only** run is *not* a green Nulang test.

If the exact-head Nulang CI run fails, keep the failure visible and diagnose the earliest compiler/runtime/persistence boundary rather than weakening the assertion or marking the demo as verified.

See [durable effect guarantees](../../docs/DURABILITY_GUARANTEES.md) and [adoption issue #809](https://github.com/nulang-org/nulang/issues/809). Nulang explicitly does not guarantee exactly-once execution of arbitrary external effects, which can commit remotely before a local result is recorded.
