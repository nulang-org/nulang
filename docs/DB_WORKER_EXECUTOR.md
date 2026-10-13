# Bounded database worker execution (experimental)

This is the **first nonblocking execution primitive** for application SQL.
It is separate from durable-actor `PersistenceStore` and reuses Nulang's
existing `BlockingExecutor`. It is not wired to source-level `DB.query`
or `DB.query_checked` yet. Those effects still use their legacy synchronous
path; do not claim that this PR makes actor database calls nonblocking.

## Execution contract

- `OwnedDbQuery { sql, params }` contains only owned values. No VM `Value`,
  register slice, actor-heap pointer or runtime borrow crosses a worker thread.
- `BoundedDbQueryExecutor::new(clients, queue_capacity_per_worker)` creates
  one dedicated thread per supplied connection. Each connection remains owned
  by its worker.
- `try_submit` uses nonblocking admission and returns a unique
  `DbQueryTicket { worker, job_id }`, or an explicit backpressure/closed error.
  Requests distribute across workers without requiring concurrent access to a
  single synchronous `postgres::Client`.
- `drain_ready` returns tagged completed results without waiting for I/O.
  A panicked worker operation becomes an error completion with the original
  ticket. Subsequent use of a poisoned connection fails explicitly.
- The optional `PostgresOwnedQueryClient` binds integer widths, float,
  boolean, text, JSON/JSONB and typed NULL values using the PostgreSQL
  statement parameter types, and shares the fallible column decoder from
  Nulang's PostgreSQL persistence implementation.

## Connection setup and security

**The caller must create and supply connected PostgreSQL clients.**
Use your deployment's configured TLS and authentication policy when creating
them. The worker adapter intentionally does not call
`postgres::Client::connect(..., NoTls)` in production.

Connection creation and pool teardown are **blocking** and should occur off
scheduler threads. Prefer bounded worker counts and small queues; queued work
and completed results have finite limits per worker.

This worker layer deliberately has no built-in retry of arbitrary SQL.
Automatic replay can duplicate writes, and a failed network response may be
an ambiguous transaction outcome.

## Critical missing pieces before enabling actor effects

1. Convert bytecode arguments to `OwnedDbParam` on the scheduler, failing
   closed on unsupported heap values, and bind requests to an actor activation
   and exact suspension token.
2. Suspend on successful admission; resume exactly once on a completion whose
   actor activation and request token still match. Ignore stale completions.
3. Add cancellation and deadlines that propagate to the driver, not merely
   drop a client-side future. Bound result bytes and stream large row sets.
4. Ensure an actor's durability journal, effect receipts, and recovery behavior
   are consistent with SQL side effects; do not imply exactly-once writes.
5. Test scheduler fairness, retries, actor cancellation/deletion, connection
   failure, worker panic, and live PostgreSQL behavior before promotion.

## Focused validation

```bash
cargo fmt --all -- --check
cargo test --no-default-features db_executor::tests
cargo test --no-default-features --features postgres postgres_query_client::tests
cargo check --locked --no-default-features --features postgres
cargo test
```

The optional live PostgreSQL integration test requires
`NU_TEST_POSTGRES_URL` pointing at an isolated test database. Its absence
means the integration test is skipped, not validated.
