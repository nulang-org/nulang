# Historical libSQL SQLite fixture

`legacy_libsql_actor_main_448cfa8c2.db.gz` is synthetic, executable test data, **not a production database**. It is the complete SQLite database file written by `LibsqlStore` on unchanged Nulang `main` commit `448cfa8c2a84588fc1c7c7d94bbb0577ad72e3d0`, using `libsql 0.10.0-pre.4`. The compressed bytes preserve the old engine's on-disk representation. No auth tokens or customer records are included.

The fixture contains an actor 42 snapshot at sequence 6 with `count=6` and `label="legacy-actor"`, two command journal entries, two workflow events, two event-sourced events, and a durable atomic transition/tail with one outbound message. The legacy writer ran with the `sqlite` feature on Rust 1.95.0, and the fixture is deliberately immutable; do not regenerate it using the replacement `rusqlite` engine.

`tests/libsql_legacy_file_compat.rs` decompresses the fixture into a disposable temporary path and opens it with the patched provider. It verifies recovery, outbox and tail state, a subsequent atomic transition, an idempotent retry, and reopening after the new write. This is a representative cross-engine fixture, **not** exhaustive coverage of corrupt databases, all historical libSQL versions, remote Turso endpoints, power-failure crash durability, or external consumer compatibility.

## Reproduce the fixture from the old engine

Check out Nulang commit `448cfa8c2a84588fc1c7c7d94bbb0577ad72e3d0` in a disposable worktree, copy `generate_main_448cfa8c2.rs` to its `tests/` directory, and run:

```sh
NULANG_LEGACY_SQLITE_FIXTURE_PATH=/tmp/nulang-main-libsql.db \
  cargo test --locked --no-default-features --features sqlite \
  --test generate_main_448cfa8c2 -- --nocapture
# Confirm the database has no active -wal/-shm sidecars; check integrity.
python3 -c 'import sqlite3; c=sqlite3.connect("/tmp/nulang-main-libsql.db"); assert c.execute("PRAGMA integrity_check").fetchone()[0]=="ok"; c.close()'
gzip -n -c /tmp/nulang-main-libsql.db > legacy_libsql_actor_main_448cfa8c2.db.gz
```

The reference fixture's SHA-256 is `90708aa6c1ada7654b4b4a2d8fadd76b26d9d756ad69bffca014384939024f2f` (gzip output); the uncompressed SQLite file's SHA-256 is `a66ad2fc9a3ff4d1ebde1cb5035ed71e549ce783040c61d6d8bc3b3c99118259`.
