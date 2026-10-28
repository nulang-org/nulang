# Isolated rusqlite recovery compatibility proof

**Experimental. Not a production store or feature toggle.** This directory
contains two *separately linked OS executables* and independent locked Cargo
manifests. The existing `LibsqlStore` remains the only supported local
durable writer, while the new `RusqliteRecoveryReader` reads existing
libSQL-created SQLite files without implementing `PersistenceStore`.
No root Cargo dependency, feature, migration, or release default changes.

## Why separate binaries?

Linking libSQL's embedded SQLite implementation and bundled `rusqlite`
into a single executable reproduced libSQL's safety assertion:
`libsql was configured with an incorrect threading configuration`.
A mixed-runtime in-process test would be invalid and unsafe. Using two
independently built executables tests the realistic migration boundary:
a file **committed and closed by libSQL** is then **opened read-only by
rusqlite** in a fresh process. Both SQLite implementations are never
loaded together in either executable.

The initial mixed-process test was **RED** at SQLite initialization.
The isolated verifier prevents this class of native symbol collision
without hiding it or weakening library checks. No production database
is modified by the verifier.

## Run

```sh
TMPDIR="$HOME/.cache" bash tools/rusqlite-local-compat/verify.sh
```

The script generates a disposable real SQLite database with two atomic
libSQL transitions and then runs six parity/fail-closed tests against that
database in the separate `rusqlite` reader test executable. It verifies:
snapshot/authority serialization, journal payloads, ordered durable
workflow events, exact epoch/sequence tail, absent actor, corrupt JSON,
corrupt digest, missing/empty database, and that readonly handles reject
writes. It also runs Clippy correctness checks against each separate
binary, requires locked manifests, and checks the actual Git revision
against `CI_COMMIT_SHA` when provided. It never requires Turso or network
credentials. The temporary database directory is removed after testing.

The reader opens existing files with `SQLITE_OPEN_READ_ONLY` and
connection-scoped `PRAGMA query_only=ON`. `PRAGMA quick_check(1)` and
explicit required-column checks fail closed on corrupt or older schemas.
All reader methods return `Result`, never silently ignore invalid
persisted state or infer empty atomic history when schema access fails.
Legacy absent authority metadata is interpreted as empty authority
(deny by default).

## Not yet proven

This **does not implement a rusqlite durable writer**. It cannot replace
the existing `PersistenceStore`, apply migrations, switch defaults,
or certify atomic updates, activation fencing, competing processes,
outbox/effect commits, crash windows, WAL power-loss durability,
file ownership, permissions, or production upgrades. A separate
rusqlite write-transaction implementation and strict multi-process
parity/recovery matrix are required before any storage cutover.

The security hotfix #2305 already removes the vulnerable libSQL remote
TLS stack and does not depend on this migration. Do not delay that
security fix or loosen its security gate to adopt rusqlite.
