# Local-only libSQL: remove the legacy TLS vulnerability

This security change is stacked on the `Cargo.lock` advisory updates in
PR #2302. It eliminates the last **vulnerability** identified in the root
RustSec audit, `RUSTSEC-2026-0382` in `rustls-webpki 0.102.8`, by removing
unmaintained libSQL remote/TLS features. No exception or audit suppression
is used. Other advisories may remain as **allowed warnings** under the
existing cargo-audit policy; review these separately.

## Supported behavior

- `sqlite` continues to use `libsql 0.10.0-pre.4` **with the `core`
  feature only**. Existing SQLite file paths, schema, WAL setup,
  `synchronous=FULL` default, atomic durable transition handling,
  activation-epoch fencing, and snapshot/journal recovery are unchanged.
- The obsolete public `LibsqlStore::new_remote` method is intentionally
  removed. An external consumer calling it must **fail at compile time**, not
  silently fall back to plaintext transport or an unrelated local file.
- There is no automatic data migration or database conversion. Never point
  an application at an existing remote URL with the local constructor.
- Remote SQL-over-HTTPS is a **separate opt-in implementation track** using
  modern `turso_serverless`. It needs full remote durability and recovery
  tests before being a replacement `PersistenceStore`; see issue #2293 and
  draft PRs #2303/#2304. The older `libsql` network driver MUST NOT be
  re-enabled as a workaround.

## Security and runtime acceptance

```sh
python3 scripts/ci/check-libsql-local-only.py
cargo audit
cargo check --locked --no-default-features --features sqlite
cargo test --locked --no-default-features --features sqlite \
  --test durable_activation_epoch \
  --test durable_compiled_store_restart \
  --test durable_effect_recovery_store \
  --test workflow_timer_process_restart_replay \
  --test workflow_effect_tail_recovery
```

A clean lockfile advisory scan and green local tests are **necessary but not
sufficient** for merge. Exact-head hosted CI, release-build compatibility,
crash/restart and security review remain mandatory. The longer-term local
backend transition to standard SQLite via `rusqlite` is a separate piece of
work; do not conflate it with this dependency-only fix.

Rollback, if approved after a release regression, must *not* re-introduce
vulnerable remote TLS. Prefer fixing local runtime compatibility or reverting
to the previously approved local-only store. Require operator authorization
before any backend or schema migration.
