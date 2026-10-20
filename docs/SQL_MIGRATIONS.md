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
