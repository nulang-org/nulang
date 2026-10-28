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

## Experimental atomic writer slice (separate process only)

The optional `reader/src/atomic_writer.rs` also implements
`RusqliteAtomicWriter::open_existing` and `commit` **inside the
separately linked rusqlite test workspace**. It remains deliberately
outside the Nulang runtime feature graph and **does not implement
`PersistenceStore`**. It is not enabled in production, included in
Nulang CLI releases, or a migration from the original local backend.

Unlike a normal SQLite helper, it accepts only a **previously created
current-schema WAL database**. It enforces `PRAGMA synchronous=FULL`,
canonical Nulang `DurableTransition::digest()`, bounded SQLite integer
conversions, and `BEGIN IMMEDIATE` ownership for the entire atomic
transaction. It checks committed tail/transition-history integrity,
activation-epoch fencing, predecessor sequence, and exact digest replay.
It writes the legacy snapshot/journal and all durable workflow, domain,
effect, and outbox rows in the same transaction as the durable tail CAS.
The CAS must change **exactly one** row, or the transaction rolls back.

The verifier's additional `atomic_writer` test binary covers:
- A transition appended to a real file previously produced by the
  independent libSQL process, with snapshot/journal/outbox/tail parity.
- Exact digest idempotence, conflicting replay, stale epoch, invalid
  transition shape, and predecessor mismatch (fail closed).
- Injected SQL failure at the late outbox stage, with no partial rows
  left in any durable table.
- Eight independent OS-process writers racing on the same actor and
  predecessor: only one distinct digest can commit.
- Two other actors safely committing through distinct OS processes.
- Invalid committed tail digest and mismatched transition-history row
  are **hard failures**, not an invitation to overwrite corrupt state.
- SIGKILL after staging uncommitted rows across all durable tables,
  before COMMIT: fresh-connection rollback and successful retry.

These tests use **synthetic data and synthetic subprocesses**, never
production actor state. The process-kill test shows rollback of an
uncommitted transaction; it does **not** establish recovery after a
power loss, after the server has committed but before the response
arrives, or under disk faults or corruption. The effect record
serialization code path exists, but there is no nonempty real
durable-effect fixture yet. A real remote Turso backend has entirely
different commit/retry semantics and is not certified by these tests.

**Further merge blockers:** full native `PersistenceStore` parity
(including all read methods and effect recovery), new/legacy filesystem
lock and migration policies, pre-commit and post-commit crash matrix,
fsync durability and disk-fault injection, large-actor concurrency,
rolling upgrades, root release integration, exact-head Woodpecker, and
operator approval. Never switch the default local backend merely
because the isolated transaction test is green.

## Staged effect/outbox recovery and ambiguous commit acknowledgement

A further **test-only** compatibility slice extends the separate-process
`RusqliteRecoveryReader` to recover:
- The **most recent matching** versioned
  `DurableEffectPersistenceRecord` by stable `DurableEffectId`.
  Records are decoded through the official `from_json` versioned
  format; corrupt newer history returns `InvalidData` instead of silently
  falling back to an older Prepared state and redispatching an external call.
- Immutable, committed **staged** `DurableOutboxMessage` entries with
  exact sequence and ordinal ordering, validated destination/behavior IDs,
  and typed persisted payloads. Reading these entries does **not** deliver,
  acknowledge, claim, or deduplicate any external message.
- Legacy and atomic domain events, without masking malformed JSON.

`tests/effect_outbox.rs` now exercises a **nonempty real**
Prepared→Completed durable effect sequence through the experimental
SQLite writer, including recovery of recorded bytes and staged messages
after closing/reopening the file. Additional negative tests corrupt newer
versioned records, payload JSON and destination IDs. An SQL-trigger
failure **after staging a real effect** proves that effect, snapshot,
journal, tail and outbox are all rolled back together.

The post-commit recovery test runs a **separate process** that calls the
actual isolated writer's COMMIT, then stops after printing a
`READY_COMMITTED_UNACKED` barrier. The parent sends SIGKILL *after*
receiving that barrier and loses the application acknowledgement. A fresh
process reads the committed tail and outbox, retries the **identical**
transition digest and verifies no duplicate rows; a different digest
at that same epoch/sequence is rejected. This is an
**unknown-acknowledgement reconciliation test**, not proof of external
provider execution/delivery or physical power-loss durability.

**Not covered:** ENOSPC/disk-full, I/O errors, failing fsync, torn pages,
power loss, filesystem and hardware-specific flush guarantees, live
multi-host SQLite sharing, malicious actor schema migrations, or complete
production `PersistenceStore` API parity. The test-only SQL-trigger
failures model transactional rollback, **not disk fault injection**. Do not
represent these isolated checks as production certification or infer
effectively-once external delivery without a provider-side deduplication
contract.
