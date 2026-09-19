# nulang-capacity-postgres

PostgreSQL persistence adapter for the provider-neutral allocation ledger in `nulang-capacity`.

## Boundary

This crate belongs to the hosted Nulang Cloud control plane, not the language/runtime hot path. It deliberately keeps PostgreSQL and Tokio dependencies outside `nulang-capacity`.

Each concrete resource provider owns one durable row:

- `provider_id` — primary key
- `generation` — optimistic concurrency fence
- `snapshot` — JSONB provider allocation state
- `updated_at` — operational timestamp

The logical initial state is generation 1 with no database row. The first successful CAS materializes generation 2. Subsequent writes use an atomic `UPDATE ... WHERE generation = expected`; unrelated providers never contend on one global generation.

Released and expired allocation IDs remain in the serialized provider snapshot as retirement tombstones, preventing delayed retries from recreating old reservations.

## Usage

The control plane should:

1. establish a `tokio-postgres` connection,
2. construct `PostgresAllocationLedgerStore`,
3. run `migrate()`,
4. load provider state,
5. plan against that exact generation,
6. apply the `nulang-capacity` mutation locally,
7. call `compare_and_swap_provider`,
8. reload and re-plan on `GenerationChanged`.

Do not hold a SQL transaction open while doing placement scoring or cloud-provider API work.
