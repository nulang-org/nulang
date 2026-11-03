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
- Nulang commits an actual durable transition. Identical replay succeeds;
  conflicting retry and stale activation epoch fail. Turso reads the
  committed tail, predecessor sequence, and complete digest.
- Turso inserts into two durable tables within an immediate transaction,
  triggers a duplicate-key error, and rolls back; neither table retains
  the partial write, and Nulang sees no phantom committed tail.
- A separate child process opens the same Nulang-initialized database via
  Turso, begins an IMMEDIATE transaction, writes provisional durable tail and
  transition rows, then exits with status 94 **without COMMIT, ROLLBACK, or
  destructors**. A fresh Nulang LibsqlStore and Turso connection must both
  observe no partial durable commit. The parent requires the exact child
  exit code, so an unrelated crash is not mistaken for a passing test.
- A **test-only Turso transition writer** derives the canonical digest using
  `DurableTransition::digest()` and checks the predecessor/activation epoch
  inside `BEGIN IMMEDIATE`, then writes snapshots, commands, durable outbox,
  transition receipt, and compare-and-swap tail atomically. It verifies
  identical replay, changed-payload conflicts, stale-epoch rejection,
  predecessor mismatch, two competing writers on separate connections, and
  libSQL reading and advancing the Turso-authored tail.
- The test writer now serializes and inserts **all Nulang-owned record
  families**: workflow events, domain events, and versioned durable effects
  (including prepared, completed, and linked compensation records), in
  addition to snapshots, command journal, outbox, transition receipts, and
  compare-and-swap tails. It checks exact payload/ordinal parity, legacy
  `LibsqlStore` recovery of the newest effect and compensation identity,
  identical-retry deduplication, and **late outbox-key collision rollback**
  leaving none of the provisional workflow/domain/effect/journal/tail rows.
  These assertions use the real legacy schema, not a synthetic look-alike.
  This remains a manually invoked test-only function, not a drop-in
  production `PersistenceStore::commit_transition` implementation.

The probe is fully local, uses only disposable SQLite files, and requires
no credentials, external service, production file, or live database.
Cargo.lock is intentionally scoped to this **test-only** workspace and
includes the existing legacy libSQL stack: it is not a security-clean
production dependency graph.

**Passing is not permission to migrate.** The probe establishes local,
file-backed SQLite compatibility and one in-process two-connection writer
race; it does **not** establish multi-process or multi-host ownership and
fencing, power-loss/fsync durability, cross-process recovery through a new
Turso-backed production `PersistenceStore`, schema/authority migration,
or real remote Turso HTTPS authentication, certificate, and transactional
semantics. The production dependency graph and its legacy libSQL/Hyper/h2
security exposure are unchanged. All of those remain separate release gates.
