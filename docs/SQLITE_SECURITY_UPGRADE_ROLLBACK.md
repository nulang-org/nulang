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

## Disposable legacy backup/upgrade/restore rehearsal (draft)

The SQLite-gated Rust regression
`test_libsql_store_disposable_legacy_backup_upgrade_reopen_restore` constructs
a disposable legacy `snapshots` and `journal` fixture, enables SQLite WAL
mode, exports a consistent immutable pre-upgrade copy through `VACUUM INTO`,
and verifies `PRAGMA integrity_check` on that copy. The test then opens the
legacy schema using the patched `LibsqlStore`, verifies the original snapshot
and journal, writes a new snapshot and journal entry, reopens the upgraded
store, and checks the new sequence and state. Finally, it restores the
pre-upgrade backup to a separate path and verifies the original sequence and
journal are intact without the post-upgrade writes.

**Qualification boundary:** Passing this test would prove the patched Rust
adapter can migrate a simple old-schema fixture and reopen a consistent
restored backup. It does **not** prove an independently built legacy Nulang
binary can read the restored copy, recovery from a hard-killed process with an
uncheckpointed WAL, crash-consistent copying of live files, or a production
rollback. Those requirements remain stop-ship until independently rehearsed.
No production data or credentials are used by this test.

## Independent old/new binary rehearsal

The pinned-binary fixture under `scripts/tests/fixtures/sqlite_cross_binary_probe.rs`
is intentionally not a normal Cargo example: use the same fixture source to
build the two different versions in disposable detached worktrees. The Python
harness creates all database files itself in a private temporary directory and
never accepts a database path. Do not invoke the Rust probe against real data.

From this repository, pin the exact pre-upgrade and candidate commits; use the
same Rust toolchain for both. For the November 26, 2026 rehearsal, the old
commit was `4cd30251c2d39ff8a8e437c1c479fa56d674363a` and the candidate was
`e0c5cf1203a27d5aeedc64f4a47bbf649b0a405d`. Replace them only deliberately
for a new release rehearsal.

```sh
repo=$(pwd)
root=$(mktemp -d)
old=4cd30251c2d39ff8a8e437c1c479fa56d674363a
new=e0c5cf1203a27d5aeedc64f4a47bbf649b0a405d
git worktree add --detach "$root/legacy" "$old"
git worktree add --detach "$root/candidate" "$new"
for version in legacy candidate; do
    cp "$repo/scripts/tests/fixtures/sqlite_cross_binary_probe.rs" \
        "$root/$version/examples/sqlite_cross_binary_probe.rs"
    (cd "$root/$version" && CARGO_TARGET_DIR="$root/target-$version" \
        cargo build --locked --no-default-features --features sqlite,tcp \
        --example sqlite_cross_binary_probe)
done
python3 scripts/verify_sqlite_cross_binary.py \
    --legacy-probe "$root/target-legacy/debug/examples/sqlite_cross_binary_probe" \
    --candidate-probe "$root/target-candidate/debug/examples/sqlite_cross_binary_probe"
```

The harness rejects identical binaries, records their SHA-256s, and checks
SQLite integrity after each stage. It verifies legacy write/reopen, upgrades
a separate copy using the candidate, writes and reopens candidate state, then
runs the **actual legacy binary** on a fresh copy of the immutable pre-upgrade
backup. It checks typed actor state, activation epoch, sequence, journal
payload, and timer workflow events, including that candidate writes do not
appear in the restored copy. The backup digest must remain unchanged.

This rehearsal passed for the pinned commits above. It complements rather
than replaces the legacy-schema regression and does not cover hard-killed
writers with uncheckpointed WAL, interrupted schema migration, production
workloads, AI-local data, or authenticated Turso. The production promotion gate
remains open.

## Required remote Turso / libSQL qualification (still open)

Use only an explicitly disposable test database and limited-scope test token;
do not use production credentials or data. Verify valid HTTPS and hostname,
invalid hostname/certificate rejection, token rejection, parameterized
`SELECT`, `INSERT`, updates, a committed transaction, explicit rollback,
reopen/restart persistence, and expected behavior of the HTTP/1.1 Hrana
pipeline. Record the exact test commit and server endpoint class **without
recording secrets**. HTTP/2-only endpoints are incompatible with this
candidate unless independently proven otherwise.

### Opt-in executable live smoke

Provision a **new** disposable database named `nulang-smoke-<unique>` with a
database-scoped, expiring write token. Never supply a shared or production
database/token. The test refuses missing confirmation, arbitrary hostnames,
plaintext, embedded credentials, ports, paths and query strings before
network access. A successful run leaves smoke rows and schema in the disposable
database; destroy only that explicitly provisioned database after recording the
result. Supply the token via the environment without printing it or including
it in command arguments or logs.

Set `NULANG_DISPOSABLE_TURSO_URL` to the new database's HTTPS/libsql URL,
`NULANG_DISPOSABLE_TURSO_TOKEN` to its test-only token, and
`NULANG_DISPOSABLE_TURSO_CONFIRM=yes`. Then run:

```sh
cargo test --locked --no-default-features --features sqlite --lib \
  disposable_turso_smoke_live_https_roundtrip -- --ignored --test-threads=1
```

The harness verifies valid authentication, rejection of an invalid token,
parameterized SQL insert/update/select, explicit commit/rollback, durable
transition idempotency and stale-epoch rejection, and reopen persistence.
The local fixture exercises the same SQL/durability contract but **does not
substitute for executing the live test**. This test does not establish
cross-process remote restart or the complete TLS negative matrix.

**Stop-ship:** Until old-store upgrade/reopen/rollback evidence and the
authenticated disposable remote suite are recorded, this document is a
runbook proposal—not a statement that production database migration is safe.

## Local query result memory budget (candidate)

The compatibility adapter buffers local SQLite query rows before exposing them
as a `Rows` iterator. To prevent an unbounded buffer, the candidate rejects
queries that would exceed **100,000 buffered rows** or approximately **64 MiB
of estimated in-memory row/value storage**, returning an error with a pagination
hint instead of silently truncating results. Text/blob bytes are measured before
copying a cell into the buffer. This is not a strict process RSS limit: SQLite
internal allocations and downstream JSON output may use additional memory.
Callers that legitimately need large result sets must use keyset pagination or
bounded `LIMIT` queries. This budget does not yet cover remote Turso queries.
