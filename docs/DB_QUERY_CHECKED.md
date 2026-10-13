# Checked database query (experimental)

`DB.query_checked` is a backward-compatible bridge from the existing
`DB.query` effect to an explicit database error contract. It is **not** a
native Nulang `Result<T, DbError>` yet.

## Contract

When compiled with `sqlite` or `postgres`, the effect accepts the same
SQL string and positional arguments as `DB.query`. It returns a
**JSON-encoded Nulang String** containing exactly one of:

```json
{"status":"ok","rows":[[1,"hello"],[2,null]]}
```

```json
{"status":"error","error":{"code":"unsupported","message":"Database queries are unavailable on this backend."}}
```

`status` distinguishes an empty successful result (`{"status":"ok","rows":[]}`)
from a failure. Rows are arrays of JSON values, not nested JSON strings.

Error codes:

| Code | Meaning |
| --- | --- |
| `invalid_input` | Missing SQL string or invalid parameters |
| `unsupported` | Current persistence backend cannot execute SQL |
| `decode_failure` | The backend produced an invalid row or unsupported column type |
| `query_failed` | Execution failed for another reason |

The message is intentionally generic to avoid exposing server diagnostics,
SQL contents, or database identifiers through this host effect. Exact errors
are logged server-side. A future driver will expose structured SQLSTATE,
retryability, and safe-to-display details.

## Authorization and compatibility

`DB.query_checked` requires exactly the same actor authority as
`DB.query`: `Other(DB, Query)`. The existing `DB.query` output format
and its legacy error behavior are unchanged.

This does **not** make database access asynchronous: the current
`PersistenceStore::query` calls can still block actor scheduler threads.
Query limits, cancellation, connection pooling, TLS configuration, and
native typed results belong to follow-up changes.

## Tests

Run the focused tests using the repository's Rust feature matrix:

```bash
cargo test --no-default-features --features sqlite checked_query_
cargo test --no-default-features --features postgres checked_query_
cargo test --no-default-features --features postgres --locked
cargo test
```

The PostgreSQL decoding tests from the parent PR additionally require
`NU_TEST_POSTGRES_URL` to point to a real isolated test database. Tests
that skip without this variable are **not** evidence of live PostgreSQL
correctness.
