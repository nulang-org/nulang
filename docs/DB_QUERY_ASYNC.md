# Asynchronous checked SQL for ephemeral actors (experimental)

This feature is the **first integration** between Nulang's `PerformAsync`
opcode and the separate bounded query executor from PR #1597. It builds on
the explicit checked-result contract from PR #1595 and PostgreSQL codec fixes
from PR #1593.

## API and deployment requirements

```nulang
// This call is valid only from an ephemeral actor's bytecode behavior.
perform DB.query_checked_async("SELECT $1::int8", 42)
```

The effect's result is a JSON-encoded Nulang String, matching
`DB.query_checked`:

```json
{"status":"ok","rows":[[42]]}
```

**The runtime does not auto-connect to a database.** The embedding host must
construct `BoundedDbQueryExecutor` with caller-configured database clients,
then call `Runtime::install_db_query_executor` before executing actors.
PostgreSQL connections must be established with an appropriate TLS policy,
off actor scheduler threads. Until a host installs the executor, the effect
returns a sanitized `unsupported` error.

Only the **bytecode VM** has asynchronous query suspension/resumption. The
native/AOT helper cannot suspend, so `DB.query_checked_async` returns a
checked `unsupported` error on that backend **without executing SQL**.

## Exact host semantics

- `DB.query_checked_async` requires the same `DB::Query` actor authority
  as both synchronous query effects. Unauthorized calls return a sanitized
  `permission_denied` envelope without submitting a query.
- The compiler emits `PerformAsync` with staged arguments
  `[SQL, compiler_generated_parameter_count, params...]`. The parameter
  count is internal to bytecode, not part of the source call. Nulang's MIR
  calling convention provides 12 staging registers, so SQL accepts up to
  **10 positional parameters** at this stage. Runtime checks arity and
  never treats unused VM registers as query parameters.
- SQL and strings must currently be module constant-pool strings. Dynamic
  heap-allocated SQL and parameter strings are rejected as `invalid_input`
  rather than allowing actor-heap pointers into a background thread.
- The callback submits a fully-owned `OwnedDbQuery` to a bounded worker.
  Backpressure produces a checked failure; the caller thread does not wait
  on network I/O, a database mutex, or queue admission.
- The scheduler retains a unique request token, worker ticket, and actor
  activation epoch. A completion only resumes an existing matching VM
  suspension and is consumed once. Deleted actors and stale activation
  completions are discarded.
- **Persistent actors and workflows are rejected before submission**:
  recording VM suspension does not make external SQL writes crash-safe,
  idempotent, or exactly-once.

## Remaining production gaps

This is draft-only pending exact-head compilation and focused tests. The
query pool still uses blocking PostgreSQL clients on dedicated workers,
not `tokio-postgres` async sockets. No built-in query deadlines,
server-side cancellation, streaming, per-query result size budget, or
operation receipts exist. A hung worker can hold a pending request and
delay scheduler quiescence indefinitely. The API does not implement
native Nulang `Result<T, DbError>` or generated sqlc-style bindings yet.

Do not promote this without testing cancellation, actor deletion/reuse,
pool replacement, fairness, queue saturation, reconnect/TLS failure,
permissions, the bytecode/native feature matrix, and a live PostgreSQL
server. Do not enable durable SQL writes until effect receipts and
recovery semantics have been designed and exercised.

## Focused validation

```bash
cargo fmt --all -- --check
cargo test --no-default-features db_effect::tests
cargo test --no-default-features checked_async_sql_lowers
cargo test --no-default-features native_backend_refuses_async_sql
cargo test --no-default-features --features postgres db_effect::tests
cargo check --locked --no-default-features --features postgres
cargo test --workspace
```

Additional live PostgreSQL checks require `NU_TEST_POSTGRES_URL` pointing
to a test-only instance. A skipped integration test does not prove the
feature works against a real database.
