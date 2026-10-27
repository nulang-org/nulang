# Manual Turso storage-format compatibility probe

This is a **standalone Cargo workspace** deliberately excluded from Nulang's
production workspace, default feature set, CI dependency graph, and lockfile.

Run manually from this directory:

```sh
cargo +1.95.0 run
```

The probe creates a disposable SQLite database with the real Nulang
`LibsqlStore` (including full journal/snapshot schema), writes one snapshot
and one journal entry, and closes the store. Turso then reads these rows and
writes a second journal entry. Reopening with the old Nulang store must
restore both entries and the correct activation epoch/state.

The test requires no external database, credentials, network service, or
production file. **Passing does not authorize migration**: transactional
durable transitions, crash/power-loss recovery, replay fencing, versioned
schemas, authority metadata, cross-process behavior, and the remote Turso
protocol are separately required gates. In particular, `turso`'s synced
database API is **not** interchangeable with legacy `libsql::Builder::new_remote`.

Do not merge this as a production driver replacement or change the
existing `sqlite` feature based on this test alone.
