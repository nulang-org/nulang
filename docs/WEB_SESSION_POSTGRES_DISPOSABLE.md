# Disposable PostgreSQL session and tenant-RLS reference

This draft extends the read-only cookie-session and resource preflight in PR #2346. It is a **disposable-only proof**, not production session persistence or a deployable HTTP handler.

The `postgres`-feature-gated `web::session_pg_reference` module implements two pre-existing traits:

- `DisposableSessionLookup`: a dedicated SELECT-only PostgreSQL login looks up a BLAKE3 digest of a high-entropy 43-character session handle, loads server-stored principal/tenant/expiry/CSRF data and excludes revoked sessions. Invalid persisted token encodings fail closed. Real session issuance MUST use cryptographically random handles and authenticated server-side persistence.
- `DisposableTenantResourcePolicy`: a separate SELECT-only database login per tenant. The application maps an already authenticated tenant to its server-owned login; it refuses incorrect role/session-user identity, superuser and BYPASSRLS connections. PostgreSQL FORCE ROW LEVEL SECURITY determines which tenant rows can be seen, while the query also checks the authenticated principal owns the requested resource.

## Isolation and guard

The harness `tests/run_web_session_pg_disposable.sh` creates a brand-new PostgreSQL 18 cluster in a random `/var/tmp/nulang-web-session-pg.*` directory and shuts it down on exit. It verifies its data directory before schema writes, binds only loopback on a non-default random port, creates fresh SCRAM passwords, and defines read-only roles. The Rust adapters refuse arbitrary hosts and default PostgreSQL port, inspect connected role/port/host and reject superuser or BYPASSRLS. A freshly generated, DB-stored random nonce verifies the connected disposable cluster without granting `pg_read_all_settings` to application roles.

The schema uses an owner-controlled `SECURITY DEFINER` function to map the PostgreSQL login's `session_user` to its tenant. The role cannot select or change the mapping directly. The row-level policy uses that mapping, **not** a client-settable GUC or an `X-Tenant-Id` header. Session-reader and tenant-reader accounts cannot write to either sessions or orders. A test superuser is used **only for disposable fixture setup and revocation simulation**; the adapters cannot obtain its credentials.

Test expectations: tenant A sees only A's rows; B sees only B's rows; A → B → A requests cannot inherit tenant authority. Forged header/form tenant claims, incorrect CSRF, revoked sessions, cross-tenant resource access and read-only role writes are denied.

## Run locally

Only run on an authorized developer/CI machine with PostgreSQL binaries installed and suitable disk capacity:

```sh
bash tests/run_web_session_pg_disposable.sh
```

The script is intentionally not connected to a pre-existing DB and **does not** support an override of the test cluster destination. Direct `cargo test --features postgres --test web_session_postgres_disposable` is not a supported validation procedure; the disposable harness provides the necessary isolated cluster and environment guards.

## Limits

**No new HTTP endpoint, WebDevServer listener, production session store, real PostgreSQL membership migration, durable revocation service, OIDC verification, secret management, pooling, TLS or deployment.** The fixture sessions use fixed test handles to make assertions reproducible. Their entropy is inadequate for real use; a production issuer must use random opaque session IDs. The session-reader fixture can SELECT the session table, which is not an appropriate production blast radius without an audited scoped lookup service. Read-only adapter credentials are demo-only and have no automatic rotation.

This proof must be reconciled with the audited real-HTTP JWT-to-PostgreSQL GET-only work (#2327 and its parent chain), and with the Web Host/Origin admission stack (#2185). The existing browser-facing and mutating HTTP surfaces remain disabled. Before promotion, run the disposable PostgreSQL suite to completion, native/WASM and security CI, independently review privileges and origin/session admission, and verify exact-head stacked PRs. Do not bypass missing checks or independently merge overlapping ancestors.
