# Native SQLite HTTP mount (experimental)

This is an opt-in **Rust host** integration that composes the SQLite store (#1828) and verified identity gate (#1830). It does **not** add an ambient Nulang Web effect, turn on a generic database REST API, or change the Web dev server's default dispatch path.

A native service using the exact compiler/runtime must explicitly call `WebDevServer::bind_runtime_with_sqlite` and supply a `WebSqliteHttpMount` built from:

- An `Arc<WebSqliteStore>` with its database on persistent local disk.
- An `Arc<dyn WebTenantTokenVerifier>` that authenticates Bearer credentials and validates tenant membership from trusted identity state.
- An `Arc<dyn WebSqliteNativeHandler>` implementing business-specific HTTP logic using only an `AuthorizedWebSqlite` capability.

The native request handler receives the parsed `HttpRequest` plus a tenant-scoped storage handle; it receives **neither** a raw tenant ID selector **nor** the unrestricted database store. All operations on the scoped capability are parameterized and tenant-key-scoped. The verifier and handler are supplied by a **trusted native host**, not by remotely uploaded Nulang source.

## Dispatch and boundaries

The explicit reserved mount prefix is `/__nulang/sqlite/`. Requests under that prefix reach the auth gate before any Nulang Web route or static fallback. Similar prefixes such as `/__nulang/sqliteevil/` are **not** intercepted.

- Valid credential and authorized tenant: native business handler runs with a scoped store.
- Missing, malformed, unauthorized or duplicated Authorization headers: HTTP 401 with a Bearer challenge.
- Identity provider unavailable/error: HTTP 503 with sanitized response (no provider or credential details).
- Application handler invalid input: HTTP 400. Other handler failures: HTTP 500.
- All responses through the mount carry `Cache-Control: no-store`. Host security headers are applied to the response as usual.
- Unmounted/default `WebDevServer::bind_runtime` callers are unchanged.

This is not a universal DB-access middleware: only the explicit host handler gets the authenticated storage handle. Normal Nulang bytecode Web routes remain separate; there is no hidden global privilege escalation, and `Web.kv_*` continues to be ephemeral.

## Testing and limitations

```sh
cargo test --locked --test web_sqlite_http_mount
cargo test --locked --no-default-features --features sqlite --test web_sqlite_http_mount
cargo test --locked --test web_sqlite_auth_gate
cargo test --locked --test web_sqlite_storage
cargo fmt --all -- --check
```

The HTTP integration test sends real TCP requests to a port-zero listener, writes and reads through the scoped capability, verifies tenant isolation with spoofed tenant headers, denies unknown/duplicated credentials before invoking the handler, checks 503 on verifier outage, and closes/reopens the native HTTP listener and SQLite store to prove committed values remain visible.

**Not yet supported:** built-in token/tenant verifier; JWT validation/revocation; role-specific permissions; typed Nulang language effects; client-facing endpoint generation; transaction coordination; Nulang Cloud WASM/Firecracker placement; proof of durability on ephemeral container volumes. A fixture verifier included in tests is not an authentication service.

The native host must supply a real token verifier and design domain-specific authorization. HTTP request storage errors must be monitored, and production persistence requires a durable mounted filesystem and backup/migration procedures. Do not enable this mount just because a request includes a claimed tenant header.
