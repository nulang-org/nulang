# PostgreSQL SQL contract preflight (Experimental)

An opt-in SQLx-inspired build-time check for literal Nulang database effects.
It does **not** change the VM, the DB effect return type, Nulang's frozen
grammar, PostgreSQL execution, actor authority, or durable replay semantics.

The optional binary is enabled only with the PostgreSQL Cargo feature:

    cargo run --no-default-features --features postgres --bin nula-sql-check -- prepare example.nula --cache .nula/sql-cache.json --allow-plaintext
    cargo run --no-default-features --features postgres --bin nula-sql-check -- check example.nula --cache .nula/sql-cache.json

Set NULANG_SQL_DATABASE_URL before the prepare command. The URL is read from
the environment and never written to the cache. The prototype uses the
existing synchronous postgres crate with NoTls. Accordingly, prepare **refuses
to connect unless --allow-plaintext is explicitly given**, and even with that
flag **rejects non-local TCP hosts and hostaddr overrides**. Use an explicit
loopback IP (127.0.0.1 or ::1), a local development database, a local SSH
tunnel, or a Unix socket. Plaintext remote database connections are forbidden.
Hostnames such as localhost are intentionally rejected to avoid accidentally
resolving to a remote address. The offline check needs no database,
credentials, or network connection.

## Scope

These explicitly written effect calls are recognized:

    perform DB.query("SELECT id FROM users WHERE id = $1::int8", user_id)
    perform DB.query_checked("SELECT id FROM users WHERE id = $1::int8", 42)
    perform DB.query_checked_async("SELECT id FROM users WHERE id = $1::int8", user_id)

The latter effects are being developed independently in draft PRs; this tool
does not enable them in the runtime. It recognizes their source spellings to
avoid needing a new checker later.

The prepare command uses PostgreSQL's extended-protocol prepare/describe
operation, which parses and analyzes the statement and provides parameter
and result-column metadata without executing the SQL statement. A successful
prepare produces deterministic, sorted JSON containing:

- Versioned contract format
- BLAKE3 hash of literal SQL (the SQL text itself is not copied into cache)
- Ordered PostgreSQL parameter type OIDs
- Ordered result column names and PostgreSQL type OIDs

The check command compares literal SQL hashes, number of call arguments,
and the type of literal scalar arguments to cached PostgreSQL metadata.
Nonliteral bind values are identified as dynamic; their types **are not**
validated by this prototype. Int, Float, Bool and String literals are
checked against a deliberately conservative subset of PostgreSQL scalar
types. Untyped `nil` binds fail closed because the current PostgreSQL runtime
does not marshal them into typed SQL NULL values. The cache is invalidated when a query changes or disappears.
Missing, stale, duplicated, or malformed metadata causes failure.

Run the normal Nulang compiler check in addition to this tool:

    nulang --check example.nula

## Important limitations

- This is an explicit **single-file** preflight, not automatic compiler or LSP
  integration. Invoke it for each relevant file using a separate cache path.
- It recognizes syntactic literal effect calls using the existing Nulang lexer.
  It is not a full expression type-checker; SQL assembled through variables
  and helpers is not supported and fails closed when directly supplied to a
  recognized effect.
- Only PostgreSQL contracts are prepared; SQLite/libSQL and other backends
  have different type systems. A PostgreSQL contract is not portable to them.
- A cached query check proves agreement with **previously captured metadata**,
  not with the live production database. Schema migration and deployment gates
  must separately verify the target database is at the expected revision.
- The cache is not authenticated. Treat metadata from untrusted sources as
  untrusted and regenerate against an approved database for sensitive builds.
- No attempt is made to infer SQL result nullability. Column names and OIDs
  are descriptive metadata, not typed Nulang row objects.
- The runtime may reject a dynamically bound parameter or a value whose
  PostgreSQL encoding differs from its compile-time literal category.
- No runtime overhead is introduced; this is a standalone developer command.

## Testing

Focused regression suite (requires the postgres Cargo feature but does not
require a running PostgreSQL server):

    cargo test --no-default-features --features postgres --bin nula-sql-check

The CI workflow now includes a PostgreSQL 16 service and a focused SQL
contract lane, which exercises real preparation and offline checking, including
missing-column, changed-query, and bind-count failures. Those checks must pass
at the **exact PR head** before promotion. The workflow uses loopback TCP only;
no external database credentials are needed.

This tool should remain Experimental until source diagnostics, type inference,
multi-file package discovery, and secure production metadata preparation are
defined and proven.
