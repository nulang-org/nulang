# Web production authentication gate (audited integration)

This is a security prerequisite for combining the audited Web HTTP stack
[#2185](https://github.com/nulang-org/nulang/pull/2185) with tenant SQL/RLS
checks [#2320](https://github.com/nulang-org/nulang/pull/2320).
It reconciles the overlapping main-based
[#2180](https://github.com/nulang-org/nulang/pull/2180); **do not merge two
copies of the same security implementation**.

## What the CLI now rejects

`nula deploy` checks the manifest and the Nulang source tree before building
or uploading an app. A package depending on `nulang-auth` or
`@nulang/auth`, or importing `@nulang/auth` from a source module, fails the
production-deploy path, including `--dry-run`. Local builds stay available.

The bundled `@nulang/auth` library is a development example: it accepts an
arbitrary `login(user)`, creates predictable `session-N` identifiers, and
stores sessions in ephemeral Web KV. Its `AUTH_COOKIE_SECRET` cloud setting
does not turn that example into cryptographically validated sessions.

Deployment IR reports only the default HTTP middleware it can truthfully
account for: `security_headers` and `request_log`. In particular,
`auth` and synchronizer-token `csrf` are **not** advertised merely because
a source file imports a package. The audited Web HTTP transport includes
separate Host/Origin/request admission checks. Those transport checks
**do not constitute authenticated principal or application CSRF tokens**.

## Verification

```sh
cargo test --offline --locked --no-default-features --lib test_infer_middleware_does_not_claim_unimplemented_auth_or_csrf
cargo test --offline --locked --no-default-features --lib test_deploy_rejects_development_auth_import_and_dependency
cargo test --offline --locked --no-default-features --features ureq --lib test_deploy_rejects_development_auth_import_and_dependency
```

The `ureq` feature exercises the actual `cmd_deploy(..., dry_run=true)`
refusal. All checks should be repeated on the exact-head GitHub CI tree.

## Remaining production release blockers

This is **not an authentication provider**. It does not introduce OIDC/JWKS,
principal verification, membership revocation, a trusted principal-to-DB-login
binding, connection-pool lifecycle isolation, or durable HTTP-backed CRUD.
The PostgreSQL RLS fixture is a separate test harness with ephemeral database
logins; request headers and cookies must never be accepted as proof of tenant
membership. Real deployment needs an independently reviewed server-side
identity provider plus fail-closed handler authorization, CSRF on mutation
paths, and negative tests through authenticated HTTP into a disposable database.

A recognized development-auth reference is denied; this guard does not
certify arbitrary third-party packages or hidden indirect imports as safe.
Other deployment gates, audits, native/WASM tests, and independent security
review still apply. Keep the integration draft until those are satisfied.
