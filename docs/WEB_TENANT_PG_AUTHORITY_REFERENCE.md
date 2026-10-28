# Disposable HTTP-request → PostgreSQL tenant-authority reference

This optional Rust adapter extends the audited Web stack (#2185), the
PostgreSQL RLS reference (#2320), and the development-auth deployment blocker
(#2321). It is **reference code only**, not a release-ready OAuth/OIDC provider
or a PostgreSQL-backed production Nulang Web handler.

## Security contract

`web::tenant_pg_reference::TenantPostgresAuthority` is enabled only under
`--features postgres` (not part of the default or minimal runtime). Its
`execute(&HttpRequest, ...)` method:

1. Accepts precisely one well-formed Bearer authorization header and asks an
   injected `TrustedTokenVerifier` to establish the authenticated subject.
   Invalid, missing, duplicate, or unmapped identities deny before SQL.
2. Resolves subject → tenant and PostgreSQL login/password from *server-owned*
   mappings. URL query strings, body values, `X-Tenant-Id`, and custom SQL
   tenant GUCs confer no authority.
3. Opens a **fresh connection per request** using the mapped login. It checks
   session_user/current_user, the protected SQL `current_tenant()` mapping,
   and rejects superuser/BYPASSRLS. No shared pooled session or `SET ROLE`
   tenant switching is introduced.
4. Runs handler SQL inside one transaction, commits only on success and
   rolls back on any query error or failed commit.

The adapter refuses remote database hosts, default port 5432, missing
disposable-process guards, or an unexpected connected network endpoint.
The privileged disposable wrapper validates the connected server's actual
`SHOW config_file` before any schema writes; tenant logins do **not** get
`pg_read_all_settings` privileges to examine that protected setting.
It uses `NoTls` **only for local disposable PostgreSQL proof**, and must not
be repurposed to connect to a production server.

## Execute test

```sh
bash tests/run_tenant_pg_authority_ci.sh
```

The wrapper launches a throwaway local PostgreSQL cluster and reads the actual
server's `SHOW config_file` before provisioning any test roles or tables.
It generates random role credentials, creates a forced-RLS schema, tests two
separate login identities, and destroys the cluster at exit. The Rust test
uses a **fake server-owned token map** to isolate the authorization-to-SQL
boundary. This does not demonstrate cryptographic token verification.

Acceptance checks include valid A/B identities, forged tenant headers and URL
params, rejected missing/invalid/duplicate/unmapped credentials, server
tenant/DB-role mismatch, cross-tenant denied writes, rollback after failed
foreign keys, and consecutive request isolation.

## Outstanding hard gates

- An independently audited identity verifier must enforce signature/JWKS,
  issuer, audience, expiration, key rotation, subject binding and revocation.
  This reference deliberately implements **no** token cryptography.
- The adapter currently accepts a parsed `HttpRequest` object in Rust tests,
  **not a socket-level HTTP request through WebDevServer**. It is not wired
  into route dispatch. A future integration must prove real network HTTP →
  verified principal → scoped database → HTTP status and protect all unsafe
  methods with Host/Origin plus synchronizer-CSRF safeguards.
- Database memberships and credentials require reviewed provisioning,
  revocation, secret rotation and TLS. There is no pooling here; a future
  pooled adapter needs verified transaction isolation, pool reset, reuse
  against alternating tenants, abort/streaming semantics and leak tests.
- Full exact-head native/WASM CI, cargo audit, release lanes and independent
  security review remain mandatory. Do not promote overlapping PR branches
  independently or deploy this test-only module.
