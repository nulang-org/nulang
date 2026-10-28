# Disposable browser-origin + server-session preflight socket contract

This isolated draft stacks on PR #2354, which stacks on #2346, #2340 and #2336.
It **does not register a Web route**, modify the production HTTP listener, enable
mutations or provide a production-ready cookie-session implementation.

## Reconciliation with the Web security review stack

The draft audited Web stack in PR #2185 defines
`web::csrf::validate_mutation_origin` (Host, Origin/Referer and Sec-Fetch-Site).
The parallel session-token stack in PR #2340 already defines `web::csrf`
for server-owned synchronizer tokens. This name collision MUST be reconciled
before any integration into production.

For isolated interoperability testing, `web::origin_admission` is a
**verbatim algorithmic copy** of the audited candidate's implementation,
with its public API preserved and its origin regression contract tested in
`tests/web_origin_admission_reference.rs`. It has not replaced the audited
implementation in production. A future integration MUST consolidate these
into ONE authoritative Origin admission policy and ONE session CSRF policy
without retaining two independently drifting origin implementations.

## Real network witness

`tests/web_session_http_disposable.rs` starts an explicitly test-owned
`TcpListener` bound to `127.0.0.1:0`, accepts one connection and reads
a bounded HTTP/1.1 request. This is a fixture, NOT a library server or
production listener. It refuses malformed framing, oversized input,
multiple Content-Length headers and all Transfer-Encoding variants.
The test-only route is `/__nulang_internal_disposable_preflight` and
accepts a POST for a **read-only** diagnostic; no VM or user handler runs.

Admission order:

1. Parse the received on-the-wire request preserving repeated headers.
2. Validate Host and Origin/Referer/Fetch Metadata against the actual listener
   address; reject cross-site/missing provenance and DNS-rebinding attempts
   **before** session lookup or validation.
3. Retrieve an opaque session using the disposable, read-only PostgreSQL role
   and verify its tenant, expiry and session-synchronized CSRF token.
4. Resolve the resource against a separate tenant-specific non-BYPASSRLS
   PostgreSQL role using FORCE ROW LEVEL SECURITY and principal ownership.
5. Only then return compiler-owned field diagnostics as `application/json`
   with `Cache-Control: no-store`. Errors return generic status/body
   without leaking private handler slots, tenant identities or token values.

The test proves only the exact positive and negative request cases defined
by the fixture; no claim is made about network-level readiness, OAuth/OIDC,
session issuance, token rotation, proxy headers, browser cookie attributes,
transactional mutation security, concurrency or complete attack coverage.

## Running

```sh
bash tests/run_web_session_pg_disposable.sh
```

The wrapper starts and verifies a newly created `/var/tmp/nulang-web-session-pg.*`
PostgreSQL cluster, then runs the database and real loopback-socket tests.
It tears down *only its own* temporary cluster on exit.
The isolated GitHub Actions `Disposable session and tenant RLS` job runs
the same harness on applicable PR changes.

## Promotion requirements

Keep the PR **draft**. Confirm local and exact-head hosted CI, reconcile
the audited #2185 Host/Origin implementation and other intersecting Web
security PRs, independently review all preflight/authorization trust boundaries,
and require transaction-level reauthorization on any eventual mutation.
Never merge the duplicate Origin implementation directly into the mainline
without consolidation. No public preflight endpoint or mutation route is
enabled by this work.
