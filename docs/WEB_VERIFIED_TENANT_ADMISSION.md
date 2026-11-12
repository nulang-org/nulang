# Verified tenant admission (experimental)

Opt-in `TenantAuthGuard` attaches a tenant identity only to explicitly protected
registered Web routes. This does not create an authentication provider, replace
database row-level security (RLS), or automatically protect older routes.

## Trusted resolver contract

An application injects a trusted `TenantSessionResolver` callback into
`WebDevServer::bind_with_tenant_guard` or the composable typed-form version
`bind_runtime_with_tenant_guard`. The resolver receives **only** a canonical
opaque 64-character lowercase hexadecimal session token from a configurable
`__Host-` session cookie. It must independently verify an unexpired,
non-revoked server-issued session, authenticated subject, and live tenant
membership; backend errors must deny access. Passing an attacker-controlled
tenant ID or a merely decoded cookie to this callback is insecure.

A protected route rejects missing, malformed, duplicate or unknown session
cookies with HTTP 401 before handler invocation, even when duplicate cookies
are split across Cookie headers. The server-scoped `Web.verified_tenant_id()`
(and Nulang wrapper `verified_tenant_id()`) returns only the tenant ID supplied
by the verified resolver. On unprotected requests, it returns an empty string.
The scope is restored even if the handler unwinds; it cannot leak to the next
request or to a different worker thread.

The opt-in generated helpers now take `records` or `record`, **not an arbitrary
tenant argument**. They read `verified_tenant_id()` from the host, require a
nonempty tenant that exactly matches `record.tenant_id`, then run the
application-owned `can_read_*` policy. Missing identity fails closed even when
a custom policy incorrectly returns true. Example call inside a protected GET:

```nulang
billboard_face_readonly_table(authorised_database_records)
```

A real application must still verify the session at issuance, enforce live
membership and record permissions, use a `Secure; HttpOnly; Path=/; SameSite`
HTTPS host-only cookie, and apply tenant isolation on every database query.
A rendered `Access denied` fragment is **not** an HTTP 403. Unsafe protected methods (POST/PUT/PATCH/DELETE) are rejected at server
startup unless a compiler-validated `GuardedFormRoute` provides independent
CSRF admission. Mutations also require transaction-level authorization. Unprotected static
paths and SSE endpoints are not covered by this opt-in guard; do not publish
private SSE rooms without an independent room permission check.

## Actual PostgreSQL RLS qualification

`tests/fixtures/web_tenant_rls.sql` creates only a disposable test table, not a
production migration. `tests/web_tenant_postgres_rls.rs` requires PostgreSQL
with a nonsuperuser, non-BYPASSRLS role and `FORCE ROW LEVEL SECURITY`.
It checks missing tenant context, cross-tenant SELECT/INSERT/UPDATE/DELETE,
bound tenant parameters, and transaction-local context reset between requests.

A dedicated CI workflow `web-tenant-rls.yml` provisions PostgreSQL 18 and an
isolated unprivileged role/database, then runs the negative suite with
`NU_TEST_POSTGRES_REQUIRED=1`. A local run without `NU_TEST_POSTGRES_URL`
is **explicitly skipped**, not evidence that RLS passed. Do not run this
fixture on production or an unrelated database.

This is not the full authenticated application -> connection -> RLS pipeline.
Actual application tables still require RLS policies, a restricted DB role,
and `SET LOCAL` of a server-authenticated tenant on the **same transaction and
connection** as the queries, with negative end-to-end tests.

## Release gates

Keep draft until exact-head hosted CI covers the guarded HTTP pathway,
PostgreSQL feature build and RLS negative tests, default/minimal/release/WASM
profiles, and security audit. The audited SQLite dependency graph still does
not support remote libSQL/Turso; resolve that separately before promotion.
No production deploy or merge is implied.
