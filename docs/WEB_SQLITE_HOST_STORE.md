# SQLite Web host store (experimental)

Nulang has a new **opt-in Rust host-side** storage primitive,
`nulang::runtime::WebSqliteStore` (requires Cargo feature `sqlite`).
It is an incremental foundation for backend apps, **not** a user-facing
Nulang Web/DB effect, generated CRUD endpoint, authorization engine, or
Supabase-compatible API.

## Host usage

A trusted native host may initialize **one** store for its lifecycle:

```rust
use nulang::runtime::WebSqliteStore;
use std::sync::Arc;

let store = Arc::new(WebSqliteStore::open("./data/web.db")?);

// Authorization MUST resolve the canonical tenant from a verified identity
// before calling storage. A URL path, header, form field, or submitted
// "tenant_id" parameter is never sufficient proof of tenant membership.
let verified_tenant_id = "authorized-tenant-id";
store.put(verified_tenant_id, "item:1", r#"{"title":"Example"}"#)?;
let item = store.get(verified_tenant_id, "item:1")?;
let items = store.list(verified_tenant_id, 100)?;
let removed = store.delete(verified_tenant_id, "item:1")?;
```

Storage operations are isolated by a composite
`PRIMARY KEY(tenant_id, key)`, use bound SQL parameters, and propagate IO
failures rather than falling back to the in-memory web KV. Each individual
write is an SQLite transaction, configured to use WAL + `synchronous=FULL`.
Values are UTF-8 strings; serialization and schema validation belong to the
trusted application layer. The bounded list API is for modest collections,
not scalable cursor pagination.

## Limits / non-goals

- No network endpoint, runtime Web effect, or session processing is added.
- **No authentication is supplied**. The storage primitive distinguishes
  tenant keys only; it cannot know whether a caller is authorized to use a
  tenant identifier.
- The existing `Web.kv_get/set/delete/all` effects **remain in-memory**.
  They must not be described as durable, or replaced implicitly.
- A single instance should be shared for the lifetime of a native host.
  It owns a Tokio runtime and a synchronized libSQL connection; constructing
  a new instance for each HTTP request defeats connection reuse.
- No implicit transactional coordination with durable actor journals,
  workflows, queues, or other databases is claimed.
- The database must live on persistent disk. A temporary/ephemeral
  container filesystem is **not** durable across container replacement.
- No Nulang Cloud WASM integration or Firecracker sandbox support is
  provided by this change.

## Validation

Use the existing Cargo feature/profile and integration test suite:

```sh
cargo test --locked --features sqlite --test web_sqlite_storage
cargo test --locked --no-default-features --features sqlite --test web_sqlite_storage
cargo nextest run --locked
```

Tests exercise read/create/update/delete, reopen durability, tenant
key separation, SQL injection-shaped keys, and validation bounds.

## Next sequence

1. Finish CI and exact-head native-route validation for PR #1827.
2. Connect this store to an **explicit**, authorized native host
   binding. The host should resolve the verified tenant ID itself,
   never accept it as a raw application-supplied string.
3. Add a real Nulang persistent CRUD reference application plus
   restart/authorization integration tests and request/response contracts.
4. Only then prove parity with Nulang Cloud's canonical WASM/runtime path.

A host that cannot prove the authenticated tenant must deny a database
operation; it must not quietly fall back to global/in-memory storage.
