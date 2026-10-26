# Application SQL versus runtime durability (experimental)

Nulang now distinguishes **application SQL** from the internal
`PersistenceStore` used for actor snapshots, journals, workflow events,
receipts and recovery. Code performing `DB.query` no longer receives implicit
access to that internal store.

## Configuration

For the CLI actor runtime or embedded host, configure application DB access
separately:

- `NULANG_STORE_PATH` configures the **internal** durable state backend.
- `NULANG_APP_POSTGRES_URL` configures the **application** PostgreSQL backend
  (requires a Nulang build with `--features postgres`).
- Library embedders may use `Runtime::set_application_database` with an
  explicit `ApplicationDatabase` implementation.

If no application database is configured, application SQL **fails closed**;
there is no fallback to the actor persistence backend. Give the application
database a distinct least-privilege role and preferably a separate database.
The Rust type separation alone cannot enforce privileges if both connections
are supplied the same administrative credentials.

The initial built-in PostgreSQL adapter is **synchronous** and uses
`postgres::NoTls`. Use it only over a trusted local transport; do not send
credentials to a remote database without TLS. Non-blocking scheduling and
remote TLS support are future hardening work, not guarantees of this release.

## Query results

Use `perform DB.query_result("SELECT ...", params...)` for an explicit result.
Its current experimental ABI is a **JSON string**, not a native algebraic
Result value:

```json
{"ok":true,"rows":[[42,null,"example"]]}
```

Or:

```json
{"ok":false,"error":{"code":"DATABASE_NOT_CONFIGURED","message":"application database is not configured"}}
```

Codes include `DATABASE_NOT_CONFIGURED`, `INVALID_SQL_ARGUMENT`,
`QUERY_FAILED`, and `INVALID_ROW_ENCODING`. Backend errors are deliberately
redacted; raw database errors can include SQL, values and credentials.
Explicit JSON decoding is currently the caller's responsibility. A typed
Nulang `Result` interface is a future compatibility migration.

The legacy `DB.query` operation still returns a JSON string containing
serialized row arrays on success and `nil` on error. **Breaking security
change:** it now requires an explicit application connection and never queries
the internal durability store. Migrate to `DB.query_result` to distinguish
database errors from missing data. Prepared parameter binding remains required;
never construct SQL by concatenating user-provided values.

## Deliberate limitations

- A host database operation can block the cooperative scheduler. Async I/O,
  connection pools, cancellation and resource quotas remain future work.
- No automatic retry: SQL mutations are not automatically reversible or
  exactly-once across network failures and durable replay.
- PostgreSQL wire codecs follow the supported types of the adapter. Unknown
  types fail with errors rather than being silently coerced.
- Do not reuse a highly privileged durability connection for application SQL.
- For multi-shard execution, each shard installs its own application database
  connection; capacity planning must account for the shard count.
