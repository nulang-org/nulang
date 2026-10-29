# Durable SQL jobs and changesets — initial experimental slice

This is an **experimental Rust host API**, not a stable source-language
`.nula` standard-library feature. See `src/durable_sql_jobs.rs`,
`src/changeset.rs`, and `tests/{durable_sql_jobs,changeset_contract}.rs`.

## Motivation

An actor mailbox send is not a durable background job. Persisted work needs
transactional admission, retry after process failure, and fenced completion.
This slice borrows Oban's transactional enqueueing and Ecto's explicit
changeset validation without adding an actor runtime or an ORM.

## SQLite/libSQL scope

Enable the `sqlite` feature. Initialize schema with
`DurableSqlJobs::install(&connection).await`. Business SQL and jobs can
share a single `libsql::Transaction`:

```rust,ignore
let tx = conn
    .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
    .await?;
tx.execute("INSERT INTO orders (id) VALUES (?1)", libsql::params![order_id])
    .await?;
DurableSqlJobs::enqueue_in(
    &tx,
    JobRequest::new("fulfillment", "ProcessOrder", &payload_json, now_ms)
        .with_unique_key(&format!("order:{order_id}")),
).await?;
tx.commit().await?;
```

The shared SQL transaction is essential. Enqueueing on a different connection
or a different database does not provide atomicity. The worker loop owns
dispatch and uses `claim`, `complete`, or `fail` with explicit Unix
millisecond timestamps. A leased job is eligible for reclaim after expiration.
The attempt counter is a fencing token; stale completions fail.

Claims are serialized with SQLite IMMEDIATE transactions. Unique keys
are enforced by a SQL unique constraint, retained indefinitely, and may only
be reused with identical job metadata. Job data includes queue, worker,
payload JSON, run timestamp and maximum attempts. Jobs eventually reach
`completed` or `discarded`. There is no automatic cleanup yet.

**Guarantees and limitations**

- Queue admission and same-connection business inserts are atomic at SQLite.
- Multiple workers can contend over the shared SQLite database.
- Execution is **at least once**, not exactly once. External providers must
  honor idempotency keys or be duplicate-safe.
- Worker claims have fixed leases: there is currently no heartbeat/renewal.
  Long tasks need a lease longer than their execution or subsequent renewal.
- The caller supplies clock values, enabling deterministic testing but also
  requiring a consistent wall-clock source in production.
- The SQL schema and operations are currently tested only against local SQLite,
  not remote Turso or PostgreSQL. No global concurrency limit, Cron engine,
  process-restart supervisor, or queue UI is included.
- The Ecto-inspired `Changeset` is a pure Rust input-validation helper.
  It never performs database writes or replaces database constraints.

## Verification

```sh
cargo test --locked --no-default-features --features sqlite --test durable_sql_jobs
cargo test --locked --no-default-features --test changeset_contract
cargo fmt --check
```

## Next integration stages

1. Surface the API through capability-checked Nulang host effects and
   standard-library bindings, with compilation/conformance tests.
2. Add idempotent external-effect adapter integration, structured job errors,
   lease renewal and crash-injection tests across two independent connections.
3. Implement a PostgreSQL backend using `FOR UPDATE SKIP LOCKED` and
   transaction-safe uniqueness; enforce backend capability parity in tests.
4. Only afterward add scheduler policies, telemetry, and a typed query/transaction
   composition API, without duplicating the native actor scheduler.
