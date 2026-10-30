# libSQL remote TLS connector

Nulang retains libSQL local and remote database interfaces, including the libsql:// URL scheme (mapped to HTTPS by libSQL). Upstream libSQL 0.10 includes a TLS connector that pins an unsupported rustls-webpki version.

Nulang instead uses libSQL's public connector API with Hyper 0.14 and supported tokio-rustls 0.26 / rustls 0.23. This preserves native certificate roots, chain verification, SNI, and hostname verification. It does not install a permissive verifier or downgrade HTTPS to plaintext.

The upstream client permits plain HTTP for development; the connector retains unauthenticated HTTP for that use. `LibsqlStore::new_remote` now rejects an HTTP destination whenever an authentication token is supplied, before allocating a runtime or opening any connection. Additionally, the token-bound Hyper connector rejects **each** non-HTTPS destination before socket creation, protecting against credential downgrades if an HTTP client later starts following redirects. It also rejects URLs with embedded userinfo, invalid schemes, and missing hosts. Authenticated remote databases must use HTTPS or `libsql://` (mapped to HTTPS). This is a destination-admission boundary, not proof that redirect behavior, server access controls, or an external Turso session is secure.

## Required validation

- cargo check --locked --no-default-features --features sqlite --lib
- cargo test --locked --no-default-features --features sqlite --lib runtime::libsql_tls::tests
- cargo test --locked --no-default-features --features sqlite --test tls_dependency_security
- cargo check --locked --all-features
- cargo audit (without ignore flags)
- Remote database integration smoke using an authorized HTTPS endpoint

Certificate-chain and hostname tests exercise local TLS handshakes and are not substitutes for real remote-database compatibility testing.
