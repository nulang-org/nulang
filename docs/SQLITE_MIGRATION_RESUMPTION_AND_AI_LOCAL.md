# SQLite migration resumption and AI-local upgrade qualification

This is a disposable-fixture rehearsal for security integration PR #2679,
not authorization to migrate production data. See
[the SQLite rollback runbook](SQLITE_SECURITY_UPGRADE_ROLLBACK.md) for operational
preflight and recovery precautions.

## Actor persistence: interrupted schema migration

Run against the candidate checkout:

```sh
cargo test --locked --no-default-features --features sqlite,tcp \
    --test sqlite_migration_resume -- --nocapture
```

The regression uses the production `LibsqlStore` to resume 15 committed
partial-schema states (the six optional snapshot columns and the auxiliary
tables, including the empty and complete prefixes). It checks snapshot and
journal preservation, subsequent writes and reopen, schema and integrity.

On Unix, a subprocess starts an uncommitted SQLite transaction containing
DDL and DML, forces dirty pages into the WAL, signals readiness, and is
killed with SIGKILL. The actual production store is the first opener after
the kill; it must recover the last committed state, finish the migration,
persist a new snapshot, and reopen with integrity intact and no leaked
uncommitted table. The subprocess entrypoint is an ordinary test that
returns before touching any database unless the parent provides its private
fixture directory. This tests an interrupted SQLite transaction, not a
failure injected at every production migration instruction or power loss.

## AI-local: independent legacy and candidate binaries

Use separate detached worktrees and target directories to avoid reusing a
binary compiled from the other source revision. From a checkout containing
this fixture:

```sh
repo="$(git rev-parse --show-toplevel)"
root="$(mktemp -d)"
legacy=4cd30251c2d39ff8a8e437c1c479fa56d674363a
candidate=0b6bf5a560be6fbcafd613001ccfe0fb69f00772
git worktree add --detach "$root/legacy" "$legacy"
git worktree add --detach "$root/candidate" "$candidate"
for version in legacy candidate; do
    mkdir -p "$root/$version/crates/nulang-ai-local/examples"
    cp "$repo/scripts/tests/fixtures/ai_local_sqlite_probe.rs" \
        "$root/$version/crates/nulang-ai-local/examples/ai_local_sqlite_probe.rs"
    (cd "$root/$version" && CARGO_TARGET_DIR="$root/target-$version" \
        cargo build --locked -p nulang-ai-local --example ai_local_sqlite_probe)
done
python3 "$repo/scripts/verify_ai_local_sqlite_cross_binary.py" \
    --legacy-probe "$root/target-legacy/debug/examples/ai_local_sqlite_probe" \
    --candidate-probe "$root/target-candidate/debug/examples/ai_local_sqlite_probe"
```

The harness accepts executable paths only, creates disposable databases,
rejects identical binaries, makes and hashes an SQLite online backup, and
uses distinct copies for upgrade and rollback. The real AI-local store
writes and reopens graph, task, and conversation data; the candidate checks
and modifies legacy data; the legacy binary checks the untouched original
and a separate restored backup. Integrity and backup immutability are
checked. Never invoke the Rust probe on real user data. Rollback restores
the pre-upgrade backup rather than assuming forward-written data can be
downgraded.

## Qualification evidence (2026-11-26)

- Legacy revision above: rusqlite 0.32; candidate revision above: rusqlite 0.41.1.
- Independently built probe SHA-256 (legacy):
  `bb46b4623508d21142616cd1acc926f7b781f3cb5212f9030f42179911c508eb`.
- Independently built probe SHA-256 (candidate):
  `9cf8be176ec72755f9b2a5122598be2d1b08d91ac0d18ad2784375ed7c6b009b`.
- Cross-binary rehearsal passed, including legacy restored-backup reopen.
- All 15 partial-schema fixtures and the Unix uncommitted-DDL SIGKILL
  recovery passed with the actual candidate store.

These results cover the tested fixture data and local engine only. They do
not qualify production databases, live authenticated Turso, or remote
synchronization, and do not substitute for the integration release gates.
