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

The probe is fully local, uses only disposable SQLite files, and requires
no credentials, external service, production file, or live database.
Cargo.lock is intentionally scoped to this **test-only** workspace and
includes the existing legacy libSQL stack: it is not a security-clean
production dependency graph.

**Passing is not permission to migrate.** Multi-process contention,
crash and power-loss durability, real Turso-backed commit/CAS behavior,
durable outbox/effects, recovery fencing across restarts, schema/authority
migration, and remote Turso authentication and transaction semantics
must pass their own recovery and security gates before any replacement.
