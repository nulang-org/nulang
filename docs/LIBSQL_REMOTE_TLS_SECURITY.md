# libSQL remote TLS connector

Nulang retains libSQL local and remote database interfaces, including the libsql:// URL scheme (mapped to HTTPS by libSQL). Upstream libSQL 0.10 includes a TLS connector that pins an unsupported rustls-webpki version.

Nulang instead uses libSQL's public connector API with Hyper 0.14 and supported tokio-rustls 0.26 / rustls 0.23. This preserves native certificate roots, chain verification, SNI, and hostname verification. It does not install a permissive verifier or downgrade HTTPS to plaintext.

The upstream client permits plain HTTP for development; this connector retains that existing behavior **only without an authentication token**. `LibsqlStore::new_remote` now rejects bearer-token requests over `http://` before initializing a runtime or opening a socket; TLS-backed `https://` and `libsql://` endpoints remain supported. URLs with inline userinfo credentials are also rejected. Production endpoints must use HTTPS or libsql://.

## Required validation

- cargo check --locked --no-default-features --features sqlite --lib
- cargo test --locked --no-default-features --features sqlite --lib runtime::libsql_tls::tests
- cargo test --locked --no-default-features --features sqlite --test tls_dependency_security
- cargo check --locked --all-features
- cargo audit (without ignore flags)
- Remote database integration smoke using an explicitly authorized HTTPS/libsql endpoint and a read-only account:

  ```sh
  # Inject these environment variables through your approved secrets manager.
  # Do not put credential values in command arguments, shell history, or test logs.
  # NULANG_LIBSQL_SMOKE_URL and NULANG_LIBSQL_SMOKE_TOKEN must already be set.
  cargo test --locked --no-default-features --features sqlite --lib \
    authenticated_remote_https_readonly_smoke -- --ignored
  ```

The opt-in test uses the same verified TLS connector and executes only `SELECT 1`.
It deliberately does **not** call `LibsqlStore::new_remote`, because that
constructor initializes Nulang tables (`CREATE TABLE IF NOT EXISTS`) and is not
suitable for a read-only production smoke. Never run against an endpoint
without explicit authorization; a test passing on a different head does not
certify later changes. The probe does not print credentials or the endpoint.

Certificate-chain and hostname tests exercise local TLS handshakes and are not substitutes for real remote-database compatibility testing.
