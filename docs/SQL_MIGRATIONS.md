# SQL Migration Scaffolding (Experimental)

Nulang application packages can generate ordered, forward-only SQL migration source files:

```bash
nulang nula generate migration create_customers
# Creates db/migrations/000001_create_customers.sql
nulang nula generate migration add_email_index
# Creates db/migrations/000002_add_email_index.sql
```

Each file contains a commented stub into which the application developer writes SQL. File sequence numbers begin at 000001. The generator rejects invalid slugs, duplicate existing versions, malformed SQL filenames and symlinked migration directories. It creates files with exclusive create semantics, so another process cannot silently overwrite a migration.

**Important: This command only generates source files.** It does not open a database connection, run SQL, change schema state, or mark versions as applied. Do not treat generated files as applied migrations, and do not execute them automatically during deployment.

A production database migration runner will require a per-database schema ledger, file checksums, concurrency/locking, crash-recovery semantics, PostgreSQL versus SQLite DDL behavior, drift detection, and operational rollback/recovery policies. The portable Nulang language runtime and Nulang Cloud should share contracts rather than independently deciding schema state. Application SQL schema changes are also distinct from Nulang actor/workflow state migration semantics.

Recommended adoption until the runner is qualified: review and apply SQL with a trusted database-native migration tool, and keep its applied schema ledger authoritative.


## Checksum plans and external applied-ledger verification

```bash
nulang nula db plan
nulang nula db plan --json
nulang nula db verify --ledger trusted-applied-export.json
```

`db plan` reads the exact SQL file bytes, validates numbered filenames, rejects missing or repeated sequence numbers, and reports an existing-workspace BLAKE3 digest for each file. It does **not** query a database, decide which migrations have run, or create a ledger. Digests include whitespace and line-ending changes.

`db verify --ledger` reads an **operator-provided export from the authoritative database ledger**, in this JSON shape:

```json
{
  "schema_version": 1,
  "applied": [
    {
      "version": 1,
      "file": "000001_create_customers.sql",
      "checksum_blake3": "<64 lowercase hexadecimal characters from nula db plan>"
    }
  ]
}
```

The applied entries must be an exact prefix of the source sequence, including each version, filename and checksum. Renamed/modified/missing source migrations, noncontiguous versions, out-of-order ledger entries, unsupported JSON versions, oversized files, and symlinked source SQL files or ledger exports are rejected.

**Trust boundary:** This command does not authenticate the ledger export, connect to the target database, or certify that a migration executed. A forged, stale, or wrong-database snapshot could pass. Operators must obtain the export directly through their trusted database access and verify provenance. Neither command applies SQL or authorizes Cloud rollout. Symlink validation is not a defense against hostile concurrent filesystem mutation.

### Future execution gates

Before offering `db migrate`, implement authoritative schema history in the target database, checksums stored within the same transaction as successful schema changes where the engine permits it, concurrency-safe migration claims, rollback/recovery policy, and separate transaction/error behavior for SQLite and PostgreSQL. Nulang Cloud must not infer migrated state from this source-only plan or an untrusted JSON file.

## PostgreSQL transaction backend (experimental, `postgres` feature)

The `postgres` feature adds `package::postgres_migrations::apply_postgres_migrations(root, client)`. It accepts an **existing `&mut postgres::Client`** selected and authenticated by the application or operator, rather than accepting plaintext credentials or constructing an insecure connection for the CLI. There is **no PostgreSQL apply CLI command** in this increment, and Nulang Cloud deployment does not invoke it automatically.

The runner preflights exact SQL BLAKE3 bytes, permits one supported `CREATE TABLE`, `CREATE [UNIQUE] INDEX` or `ALTER TABLE` statement per migration, and rejects multi-statement scripts, transaction-control commands, changes to its reserved ledger identifier, and `CONCURRENTLY`. It uses one PostgreSQL transaction, a fixed transaction-scoped advisory lock, bounded lock/statement timeouts, and an authoritative ledger in `public.__nulang_schema_migrations`. Applied version, filename and digest checks run inside the lock; pending DDL and ledger inserts commit atomically or roll back as one batch.

**Integration gate:** Live tests are ignored by default because they mutate schema. To run the explicit test, create a disposable PostgreSQL database whose name starts with `nulang_migrations_test_`, set `NULANG_TEST_POSTGRES_MIGRATIONS_URL` for that local disposable database, and run the ignored `applies_and_replays_against_disposable_postgres` test with `--features postgres -- --ignored`. The test drops and recreates only its reserved test objects, so never point it at a valuable database. No live PostgreSQL integration was executed during authoring.

This first increment does not include a ready-to-use TLS-enabled PostgreSQL CLI, tenant/schema isolation, zero-downtime online index handling, arbitrary SQL scripts, statement-level audit, or operational deployment approval. Environment-specific credentials, TLS, roles and backups remain the operator's responsibility. Transaction and concurrency claims require passing real database tests before promotion.
