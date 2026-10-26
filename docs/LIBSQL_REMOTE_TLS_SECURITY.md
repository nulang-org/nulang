# libSQL remote TLS connector

Nulang retains libSQL local and remote database interfaces, including the libsql:// URL scheme (mapped to HTTPS by libSQL). Upstream libSQL 0.10 includes a TLS connector that pins an unsupported rustls-webpki version.

Nulang instead uses libSQL's public connector API with Hyper 0.14 and supported tokio-rustls 0.26 / rustls 0.23. This preserves native certificate roots, chain verification, SNI, and hostname verification. It does not install a permissive verifier or downgrade HTTPS to plaintext.

Before passing bearer credentials to libSQL, `LibsqlStore::new_remote` validates the remote URL and fails with `InvalidInput` for non-loopback plaintext HTTP, unsupported schemes, userinfo, fragments, control characters, and ambiguous authorities. HTTPS, `libsql://`, and existing bare-hostname HTTPS aliases are retained. Plain HTTP is allowed only for explicit loopback destinations (`localhost`, 127.0.0.0/8, or `::1`) for local development; remote servers must use HTTPS. Rejections happen before DNS, TCP, TLS, or token transmission, and validation errors never echo the token or URL credentials.

## Required validation

- cargo check --locked --no-default-features --features sqlite --lib
- cargo test --locked --no-default-features --features sqlite --lib runtime::libsql_tls::tests
- cargo test --locked --no-default-features --features sqlite --test tls_dependency_security
- cargo test --locked --no-default-features --features sqlite --lib libsql_remote_url_validation_tests
- cargo test --locked --no-default-features --features sqlite --test libsql_remote_url_admission
- cargo check --locked --all-features
- cargo audit (without ignore flags)
- `cargo test --locked --no-default-features --features sqlite --test turso_remote_smoke` (runs the fail-closed admission test; the live test stays ignored)
- Authenticated remote Turso smoke, run **only** against an authorized, dedicated disposable database named `nulang-smoke-*` using HTTPS or `libsql://`:
  - Provision an isolated database and a short-lived, limited credential; never point the test at production/shared data.
  - Supply `NULANG_TURSO_E2E_URL`, `NULANG_TURSO_E2E_TOKEN`, and `NULANG_TURSO_E2E_CONFIRM=DISPOSABLE_DB_SCHEMA_CHANGES_OK` as protected environment variables. Do not put tokens on the command line, in a shell history, or in logs.
  - Execute `cargo test --locked --no-default-features --features sqlite --test turso_remote_smoke -- --ignored --exact authenticated_disposable_turso_snapshot_survives_reconnect` in the authorized environment.
  - The smoke creates/migrates Nulang tables and writes one unique actor snapshot, drops the client, reconnects, and verifies the value. It intentionally does not delete records; retire the **entire disposable database** through the normal approved teardown process.
  - Record exact Git SHA, endpoint hostname (no token), pass/fail boundary, and disposable-database retirement evidence. Never claim the remote gate passed without observing this command exit successfully.

Certificate-chain and hostname tests exercise local TLS handshakes and are not substitutes for real remote-database compatibility testing.
