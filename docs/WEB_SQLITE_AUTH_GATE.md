# Web SQLite auth gate — experimental native host API

This is a Rust **host-side** authorization adapter. It requires the `sqlite` feature and PR #1828. It is not wired into the Web HTTP server or Nulang-language effects; it does not create authenticated HTTP endpoints or Cloud parity.

The gate takes a `WebSqliteStore` and a mandatory `WebTenantTokenVerifier`. The verifier MUST authenticate credentials and resolve a canonical tenant ID using a trusted identity and tenant-membership provider. No built-in verifier is provided; no bypass or permissive default exists.

The HTTP adapter passes parsed header pairs (with duplicates preserved) to `WebSqliteAuthGate::authorize_headers`. A single Authorization: Bearer credential is required. Unknown, empty, duplicate or malformed authorization fails closed. The gate does not read X-Tenant-ID, X-Forwarded-Tenant, path, query, or form fields to select a tenant.

Example host use (using a real, independently implemented verifier):

```rust
use nulang::runtime::{WebSqliteAuthGate, WebSqliteStore};
let store = WebSqliteStore::open("./data/web.db")?;
let gate = WebSqliteAuthGate::new(&store, &trusted_verifier);
let authorized = gate.authorize_headers(&parsed_http_header_pairs)?;
authorized.put("item:1", "payload")?;
let value = authorized.get("item:1")?;
```

The resulting `AuthorizedWebSqlite` handle has no tenant selector in its get, put, delete or list APIs. It is intended for a **single request**, not indefinite caching. The underlying raw store remains a privileged native-host API; never pass it to untrusted application code.

## Limitations

- A verifier that trusts unchecked JWT claims or user-supplied tenant headers defeats this boundary. The verifier MUST validate signature, expiry, issuer, audience, and membership, and handle revocation appropriately.
- Verifier backend failures deny access and return sanitized diagnostics. An HTTP adapter must distinguish denied credentials (401/403) from unavailable verification (503) without disclosing provider messages.
- Native HTTP server wiring, middleware, session lifecycle, CSRF controls, roles, authorization policies, and Nulang-language CRUD endpoints are intentionally out of scope.
- Storage errors never silently fall back to in-memory Web KV.
- End-to-end authenticated HTTP tests and server restart recovery are still required before production use.

## Validation

```sh
cargo test --locked --test web_sqlite_auth_gate
cargo test --locked --no-default-features --features sqlite --test web_sqlite_auth_gate
cargo fmt --all -- --check
```

Promotion is blocked until these commands and the parent storage PR's native tests pass in CI.
