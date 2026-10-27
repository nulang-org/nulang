# Manual Turso storage-format compatibility probe

This standalone Cargo workspace is excluded from Nulang's production workspace,
default feature set, CI dependency graph, and root lockfile.

Run manually from this directory:

```sh
cargo +1.95.0 run --locked
```

The test uses the actual production Nulang `LibsqlStore` schema and checks:

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

This is a **single-process, single-writer** compatibility test. It does not
prove two-writer cross-process races, crash consistency after forced power
loss, storage recovery under I/O failure, authenticated remote transport,
independent schema bootstrap, or the complete Nulang `PersistenceStore`
API. No production service should use this prototype.
Cargo.lock is intentionally scoped to this **test-only** workspace and
includes the existing legacy libSQL stack: it is not a security-clean
production dependency graph.

**Passing is not permission to migrate.** Multi-process contention,
crash and power-loss durability, real Turso-backed commit/CAS behavior,
Turso-native durable commit/CAS, restart and concurrent-owner fencing,
full actor recovery and workflow/outbox effect delivery semantics,
schema/authority migration, and remote Turso authentication and transaction semantics
must pass their own recovery and security gates before any replacement.
