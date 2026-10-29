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
- Remote database integration smoke using an authorized HTTPS endpoint

Certificate-chain and hostname tests exercise local TLS handshakes and are not substitutes for real remote-database compatibility testing.
