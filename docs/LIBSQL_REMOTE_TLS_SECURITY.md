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
- Remote database integration smoke using an authorized HTTPS endpoint

Certificate-chain and hostname tests exercise local TLS handshakes and are not substitutes for real remote-database compatibility testing.
