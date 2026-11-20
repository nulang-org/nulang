# Local SQLite security upgrade and rollback runbook

Status: **candidate procedure for PR #2679; production migration is NOT qualified**.
This applies to Nulang actor persistence opened through `LibsqlStore::new` /
`with_sync_mode` and the AI-local SQLite backend. It does not apply to JSON
snapshots, in-memory databases, NuDB, or hosted Turso databases.

## Compatibility boundary

PR #2679 replaces the local libSQL SQLite engine with a single patched
`rusqlite` / `libsqlite3-sys` provider. The on-disk database is expected to
remain SQLite-compatible, but a matching file format is **not** proof of
application-level compatibility. `LibsqlStore::ensure_tables` may add
columns to legacy `snapshots` tables on open. The local store applies
`journal_mode=WAL` and defaults to `synchronous=FULL`. A downgrade after
opening or writing with the candidate must not be assumed safe.

Remote Turso/Hrana remains a separate HTTPS-only connection. Its transport
changes from the legacy Hyper HTTP/2-capable dependency to HTTP/1.1 only.
**Loopback HTTPS with an intentional 401 response is not evidence of working
authenticated Turso SQL operations.**

## Preflight: must be done against disposable copies first

1. Record the exact old binary version/commit, the candidate commit, host
   architecture, SQLite database locations, journal/synchronous settings,
   and the expected actor IDs, sequence numbers, durable effect counts, and
   workflow states. Inventory every Nulang process that can write each store.
2. Stop **all** writers, actors, workers, and processes sharing a database;
   verify they actually exited. Do not copy an actively changing SQLite WAL
   database by copying only its main `.db` file.
3. Using an authenticated local operator environment, create an immutable,
   access-controlled backup with SQLite's online backup API or `sqlite3`
   `.backup`, while writers remain stopped. Include any external actor
   artifacts stored outside SQLite. A cold copy of the main DB with its
   `-wal`/`-shm` companions is a fallback only with writers fully stopped
   and the copy performed consistently.
4. On a **separate restored copy**, verify `PRAGMA integrity_check` returns
   `ok`; inspect `PRAGMA journal_mode`, `PRAGMA user_version`, table
   definitions, expected row counts and durable sequence continuity. Record
   the checksum and permissions of the original immutable backup.
5. First rehearse restart/recovery with the **old** binary against one copy.
   Then rehearse with the candidate against a **different** copy. Compare
   actor state, journal/replay, activation fencing, transaction rollback,
   string/blob/NULL binding, and AI-local read/write behavior. Include a
   database with an uncheckpointed WAL and legacy optional columns.
6. Reopen each upgraded copy after a clean process restart. Exercise an
   intentionally failed transaction and compare state before/after.
   No production migration is authorized without these results and a
   documented independent review.

## Suggested disposable SQLite inspection

Run only on a private, stopped, **copied** database, never against an unknown
production store. Paths shown are placeholders.

```sh
sqlite3 /path/to/copied-actor.sqlite 'PRAGMA integrity_check;'
sqlite3 /path/to/copied-actor.sqlite 'PRAGMA journal_mode;'
sqlite3 /path/to/copied-actor.sqlite 'PRAGMA user_version;'
sqlite3 /path/to/copied-actor.sqlite '.schema snapshots'
sqlite3 /path/to/copied-actor.sqlite '.schema journal'
sqlite3 /path/to/copied-actor.sqlite '.schema workflow_events'
```

SQLite's `user_version` is a diagnostic field, **not** a substitute for
checking the application schema or actual restored actor behavior. Do not
modify it to imply an upgrade succeeded.

## Promotion and rollback

- Promote only from a tested *exact-copy* rehearsal after all required
  release checks, independent review, and remote live-service qualification.
  Keep the old binary and pre-upgrade backup in a protected location.
- On any failed integrity check, incomplete schema migration, wrong
  actor/sequence/authority state, or durability mismatch, **stop** the
  candidate. Preserve logs and the affected database; never automatically
  continue writing to a questionable store.
- To roll back, stop every writer, restore the **pre-upgrade** immutable
  backup to a new clean directory, validate integrity and expected durable
  state, and only then launch the pinned old binary against the restored
  directory. Never simply downgrade the executable while continuing to
  write to a database that the newer code may have changed.
- Writes committed after the backup require a separately verified replay or
  reconciliation procedure. Restoring the backup alone **loses those writes**.
  If safe replay cannot be proven, defer rollback/promotion and escalate.

## Required remote Turso / libSQL qualification (still open)

Use only an explicitly disposable test database and limited-scope test token;
do not use production credentials or data. Verify valid HTTPS and hostname,
invalid hostname/certificate rejection, token rejection, parameterized
`SELECT`, `INSERT`, updates, a committed transaction, explicit rollback,
reopen/restart persistence, and expected behavior of the HTTP/1.1 Hrana
pipeline. Record the exact test commit and server endpoint class **without
recording secrets**. HTTP/2-only endpoints are incompatible with this
candidate unless independently proven otherwise.

**Stop-ship:** Until old-store upgrade/reopen/rollback evidence and the
authenticated disposable remote suite are recorded, this document is a
runbook proposal—not a statement that production database migration is safe.
