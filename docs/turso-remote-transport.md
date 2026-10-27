# Remote Turso SQL-over-HTTPS preflight (experimental)

This migration slice introduces the optional Cargo feature
`turso-remote-probe`. It is deliberately **not** enabled by `full` or
`release-cli`, does not replace `LibsqlStore`, and does not implement
`PersistenceStore`. Existing local/remote database behavior remains
unchanged.

## Example: explicit, read-only connectivity check

```rust,no_run
# async fn example(url: &str, token: &str) -> std::io::Result<()> {
use nulang::runtime::TursoRemotePreflight;

// Caller owns credential acquisition/rotation and must not log the token.
let probe = TursoRemotePreflight::new(url, token)?;
probe.verify_tls_auth().await?;
# Ok(())
# }
```

Build with `--no-default-features --features turso-remote-probe` to
exercise the modern `turso_serverless 0.1.3` SQL-over-HTTP client
without enabling the legacy libSQL persistence implementation.

The constructor performs **no** network requests, rejects
plaintext `http://`, embedded user info, token-bearing query strings,
fragments, non-root paths, empty/control-character credentials, and
invalid URLs. `libsql://` and `turso://` are explicitly normalized to
HTTPS. Debug output redacts the token. The only network operation
is an explicit `SELECT 1` query using bearer authentication; no
schema changes, durable transitions, or side effects are issued.

## Release gates still missing

This isolated transport preflight is **not** a complete storage adapter.
It cannot certify remote atomic transactional batches, predecessor
CAS/activation fencing, idempotent recovery, bounded retry/backoff,
remote service failure handling, server-enforced TLS identity or token
rotation under load, durable outbox/effect delivery, cold restart,
or clean-room restore. Those need real, least-privilege, disposable
remote Turso integration tests and a production `PersistenceStore`
implementation before rollout.

The root lockfile **still includes** `libsql 0.10.0-pre.4` and the
vulnerable legacy `rustls-webpki 0.102.8`, even though the new
serverless client uses newer Reqwest/TLS packages. The complete
unmodified `cargo audit` must reach zero actionable vulnerabilities
before any release. Keep feature off by default and do not weaken
RustSec enforcement. See security migration issue #2293 and the
standalone local recovery proof stack #2294/#2296/#2299/#2300/#2301.

## Local verification

```sh
cargo test --locked --no-default-features --features turso-remote-probe --test turso_remote_preflight
cargo check --locked --no-default-features
cargo audit
```

A real service query is only attempted when the caller explicitly
invokes `verify_tls_auth`; the automated tests perform no external
network requests and do not require production tokens.

## SQL-over-HTTP transaction wire contract (synthetic loopback)

The feature-gated `tests/turso_remote_atomic_wire.rs` exercises the *actual
pinned* `turso_serverless 0.1.3` HTTP client against an in-process
127.0.0.1 fixture (never against a remote account). The fixture is allowed
to use local plaintext HTTP **only inside the test**; the public
`TursoRemotePreflight` constructor continues to reject all HTTP URLs.

The six contract tests verify:

1. `Connection::transactional_batch(..., Immediate)` sends
   `BEGIN IMMEDIATE`, conditionally executes two synthetic statements, and
   gates `COMMIT` on success or `ROLLBACK` on failure. The bearer token is
   synthetic, and SQL values are serialized as bind parameters.
2. A simulated SQL constraint error produces a failed result, not success.
3. A disconnected HTTP response cannot be reported as a confirmed commit.
   This **does not** resolve the *actual* server's potentially ambiguous
   commit outcome after a lost response; the recovery protocol must
   reconcile persisted digest/tail on a fresh authenticated connection.
4. An **interactive** `BEGIN IMMEDIATE` transaction checks the tail
   compare-and-swap affected-row count *before committing*; zero or two
   updated rows lead to an explicit `ROLLBACK`. Exactly one row permits
   `COMMIT`. Baton continuity across the HTTP requests is checked.

**Important boundary:** even an otherwise atomic
`transactional_batch` is insufficient for durable fencing if a CAS
`UPDATE` matches no rows: SQL considers the update successful, and the
client would commit unless the application verifies the affected-row count.
A production remote durable writer must use an interactive transaction that
checks exactly one changed tail row **before** commit (or a separately proven
server-side constraint with equivalent rollback semantics).

The loopback response is synthetic. It does not model a real SQL engine,
server-side transaction locks, host failover, actual response-loss recovery,
remote TLS certificate validation, token rotation, or multi-machine races.
It must not be cited as production authorization.

The dedicated path-scoped self-hosted
`.woodpecker/turso-remote-wire.yml` lane invokes
`scripts/ci/turso-remote-wire.sh`, checks the exact revision (via
`CI_COMMIT_SHA`), and caps execution at 40 minutes. A checked-in
Woodpecker pipeline is **not proof it has been run**; inspect the actual
exact-head pipeline and server-side PR authorization settings before
calling it a passing hosted gate. Root RustSec and release checks remain
independent and mandatory.
