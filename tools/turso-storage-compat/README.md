# Manual Turso storage-format compatibility probe

This standalone Cargo workspace is excluded from Nulang's production workspace,
default feature set, CI dependency graph, and root lockfile.

Run manually from this directory:

```sh
cargo +1.95.0 run --locked
```

The test uses the actual production Nulang `LibsqlStore` schema and checks:

- **Schema and authority identity:** synthetic, non-credential authority-token
  strings, canonical actor schema name and CRDT field identity must survive
  libSQL -> Turso reads and Turso -> libSQL -> Turso durable transitions.
  Existing legacy snapshots with no authority remain readable; these
  sentinels are never interpreted as valid capabilities or production grants.
- Original Nulang store writes a snapshot and journal; Turso reads state,
  activation epoch, sequence, and payload; Turso writes another journal row;
  Nulang reopens and reads both records.
- Nulang commits an actual durable transition containing a workflow event,
  domain event, prepared durable external effect and outbound actor message.
  Identical replay succeeds; conflicting retry and stale activation epoch
  fail. Turso reads each real-schema record, checks event ordinals, effect
  envelope identity and outbound destination/behavior/payload, and reads the
  committed tail, predecessor sequence and full digest. The original libSQL
  store can still reopen and recover its workflow/domain/effect records.
- Turso inserts into all six durable-transition tables within an immediate
  transaction, triggers a duplicate-key error, and rolls back. All six
  tables must discard the partial actor-31 write; Nulang sees no phantom tail.

The probe is fully local, uses only disposable SQLite files, and requires
no credentials, external service, production file, or live database.

## Experimental local Turso commit (opt-in, test only)

The separate `src/turso_local_commit.rs` module is called **only** by the
manual compatibility probe. It is not a Nulang `PersistenceStore` adapter,
and its dependencies are not part of Nulang's production workspace.

It uses Nulang's canonical transition digest/structure validation, an
`BEGIN IMMEDIATE` SQLite transaction, predecessor and activation-epoch
fencing, digest-based idempotent replay, full snapshot/journal/workflow/domain/
effect/outbox inserts, a guarded tail compare-and-swap, and explicit rollback
on errors. A per-connection `PRAGMA synchronous` check refuses any mode other
than `FULL` before returning a successful durable commit.

The runner now verifies all of the following using real disposable files:

- A Turso-authored full transition (including effect and outbox) can be
  recovered using the original Nulang `LibsqlStore` after reopening it.
- Identical replay is idempotent; conflicting digest, stale epoch, and
  skipped predecessor are refused.
- A deliberately injected late duplicate transition-key failure rolls back
  all staged journal/workflow/domain/effect/outbox rows and does not advance
  the tail; valid retry after removing the test-only fault succeeds.
- Downgrading the connection to `synchronous=NORMAL` is rejected before the
  write, and the actor's tail remains absent.
- A third transition committed with original `LibsqlStore` after two
  Turso-originated transitions is visible, including the exact digest and all
  six record groups, to another fresh Turso connection.

## Cross-process recovery and competing writers (test-only)

The manual `cargo +1.95.0 run --locked` runner now additionally launches the
**same executable in separate OS processes** using an internal
`--process-worker` mode. All tests use real, disposable, libSQL-initialized
SQLite files. They are not run against production data or remote endpoints.

- **Twelve conflicting two-writer races:** two child processes wait on a
  parent-owned file barrier, attempt the same actor/epoch/sequence with
  different payloads, and must produce exactly one successful commit. A fresh
  legacy `LibsqlStore` verifies the winner's canonical digest, persisted
  payload, one atomic tail and exactly one record in *each* snapshot,
  journal, workflow/domain event, durable effect and outbox table. Losing
  attempts must leave no duplicate or partial rows. The losing writer may
  reject on SQLite lock contention or a conflicting digest; this does not
  prove retry fairness or multi-host distributed arbitration.
- **Forced termination before COMMIT:** a dedicated test-only callback signals
  the parent *after* Turso stages all rows and the guarded tail update inside
  `BEGIN IMMEDIATE`, then blocks. The parent sends a terminating SIGKILL to
  that child. A newly reopened legacy connection must find **zero** rows in
  all eight checked actor tables and no durable tail. A second child must
  then acquire the released lock and successfully commit that same transition.
- **Previous single-process checks remain active:** epoch/predecessor fencing,
  conflicting digest and duplicate replay, `synchronous=FULL` guard,
  injected late constraint rollback, and alternating Turso/libSQL/Turso
  restart-read interoperability.

Passing is evidence of **local two-process exclusion and OS-process-kill
rollback**, not proof of physical power-loss durability, two-machine
consensus, adversarial filesystem I/O failures, or remote service semantics.
The callback and child entrypoint exist only in this separate manual test
workspace, not in the production Nulang storage implementation.

The dedicated Woodpecker workflow at
`.woodpecker/turso-storage-compat.yml` runs the standalone harness on
path-scoped pull requests (including stacked draft bases), `main` pushes
touching the probe, or explicit manual events. It uses an unprivileged
Rust 1.95 container, a 40-minute shell timeout plus 30-second kill grace,
two Cargo build jobs, no credentials, strict Clippy, formatting, locked
metadata and real executable recovery tests. Forked/untrusted PRs MUST
require operator approval on the Woodpecker server; do not expose secrets
to the workflow. This added config is **not evidence it actually ran
on the exact head**: inspect an actual Woodpecker pipeline result,
head SHA and complete step logs before certification.

Root GitHub CI success alone does not run or certify this standalone
probe. Both the existing release matrix and RustSec audit gates from
#2293 remain mandatory and independent.

This remains a test-only prototype. It does not prove physical power-loss
crash consistency, authenticated remote transport, independent schema
bootstrap, complete Nulang `PersistenceStore` compatibility, or recovery
under storage I/O failure. No production service should use it.
Cargo.lock is intentionally scoped to this **test-only** workspace and
includes the existing legacy libSQL stack: it is not a security-clean
production dependency graph.

**Passing is not permission to migrate.** Beyond these local process
and restart tests, cross-host coordination, physical power-loss and I/O fault
durability, a production Turso `PersistenceStore` integration, full actor
recovery and workflow/outbox effect-delivery guarantees, independent
schema/authority migration, and remote Turso TLS/authenticated transactions
must pass their own recovery and security gates before any replacement.
