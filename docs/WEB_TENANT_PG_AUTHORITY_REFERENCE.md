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
separate login identities, and destroys the cluster at exit. The original
Rust tests use a **fake server-owned token map** to isolate authorization.
An additional integration check now signs a short-lived access token with a
deterministic **test-only** Ed25519 key, verifies its signature and audience
through the pinned verifier, and proves A/B SQL isolation. No production
private key or external identity provider is involved.

Acceptance checks include valid A/B identities, forged tenant headers and URL
params, rejected missing/invalid/duplicate/unmapped credentials, server
tenant/DB-role mismatch, cross-tenant denied writes, rollback after failed
foreign keys, and consecutive request isolation.

## Outstanding hard gates

- `web::tenant_jwt_reference::PinnedEd25519Verifier` now checks EdDSA
  signatures using a server-pinned public key, exact `kid`, `typ=at+jwt`,
  issuer/audience, signed subject, bounded `iat`/`exp`/`nbf`, and `jti`
  against an injected revocation policy. Test with
  `cargo test --locked --no-default-features --features postgres --test tenant_jwt_verifier`.
  The code does **not** fetch untrusted JWT `jku`/JWKS header URLs or accept
  unsigned/other-algorithm tokens. An unavailable revocation response is
  fail-closed (`None`); only explicit `Some(false)` allows a token. It is
  not an OIDC provider or discovery client. Production issuer trust,
  public-key distribution and rotation, durable revocation data/availability,
  session logout, and incident rekeying remain **unimplemented and
  independently reviewable**; a dummy revocation implementation is only
  used in disposable tests.
- A new **loopback-only, disposable-only** `WebDevServer::bind_disposable_tenant_authority`
  reference listener now proves the actual TCP HTTP reader, Host/Origin admission,
  pinned signed-token verification, server-owned tenant/PostgreSQL login,
  forced RLS and JSON response over socket. Run via
  `bash tests/run_tenant_pg_authority_ci.sh`, which also exercises
  `tests/tenant_http_socket_authority.rs`. It is **not installed** by
  normal `WebDevServer::bind_runtime`, `nula dev`, production `Web.route`
  or `nula deploy`; it is gated to a local `pg_virtualenv` cluster and
  binds only `127.0.0.1`.
- The reference endpoint `/__nulang_test/tenant-faces` is intentionally
  **GET-only**. Untrusted Host fails before handler execution, mutations
  with missing/foreign Origin fail HTTP 403, and even same-origin mutations
  fail HTTP 405. This avoids implying synchronizer-CSRF authorization or
  deploying unsafe mutation handlers; **no production CSRF token system is
  implemented**.
- The integration is *not* a full general-purpose Nulang VM route that can
  open an arbitrary production SQL pool. Production authorization, CSRF
  handling, revocation, credential/TLS lifecycle, transaction-to-HTTP failure
  semantics and user-facing Web route binding remain separately blocked.
- **Server-controlled verification key rollover:**
  `PinnedEd25519KeyRing` holds at most 16 uniquely identified, server-pinned
  Ed25519 public keys with a shared fail-closed revocation policy. It rejects
  duplicate, malformed and unknown `kid` values, unsigned/alternate-algorithm
  tokens and token-supplied key URLs. Overlapping old/new keys can validate
  during a deliberate rotation window; replacing the immutable server trust
  bundle without the old key immediately rejects its tokens. This is a
  **testable trust configuration contract, not production JWKS discovery,
  distribution, key management, automatic refresh or emergency rotation**.
  CI runs `tests/tenant_jwt_keyring.rs` as a separate regression suite.
- **Durable revocation reference:** `web::tenant_pg_revocation_reference::DisposablePostgresRevocation`
  checks a protected PostgreSQL `revoked_tokens(issuer,subject,jti)` table
  using a separate SELECT-only login on every token verification. Revoking a
  token in PostgreSQL denies the same signed token immediately on subsequent
  requests, including a newly constructed verifier; another user's token
  remains valid. Missing SELECT privileges, missing table, bad connection or
  unavailable PostgreSQL yield `None` and fail closed. The role has no
  UPDATE/INSERT/DELETE privilege; tests prove denied attempts.
  Run `bash tests/run_tenant_pg_authority_ci.sh` to exercise a fresh
  ephemeral PostgreSQL cluster with the additional
  `tests/tenant_pg_durable_revocation.rs` suite. This is **not** a production
  revocation service or migration: production requires audited durable
  provisioning, availability, token cleanup/retention, key rotation, secure
  TLS, operational logging, revocation writers and race/transaction review.
- Database memberships and credentials require reviewed provisioning,
  revocation, secret rotation and TLS. There is no pooling here; a future
  pooled adapter needs verified transaction isolation, pool reset, reuse
  against alternating tenants, abort/streaming semantics and leak tests.
- Full exact-head native/WASM CI, cargo audit, release lanes and independent
  security review remain mandatory. Do not promote overlapping PR branches
  independently or deploy this test-only module.
