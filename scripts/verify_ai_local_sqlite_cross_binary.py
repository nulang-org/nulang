#!/usr/bin/env python3
"""Rehearse AI-local SQLite compatibility using independent old/new binaries.

Only executable paths are accepted. All databases are private disposable
fixtures created by the harness. This is not a production migration tool.
"""
import argparse
import shutil
import sqlite3
import tempfile
from contextlib import closing
from pathlib import Path

from verify_sqlite_cross_binary import digest, integrity, run


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--legacy-probe", required=True, type=Path)
    parser.add_argument("--candidate-probe", required=True, type=Path)
    args = parser.parse_args()
    legacy = args.legacy_probe.resolve(strict=True)
    candidate = args.candidate_probe.resolve(strict=True)
    old_digest, new_digest = digest(legacy), digest(candidate)
    if legacy == candidate or old_digest == new_digest:
        raise SystemExit("Refusing identical old/new binary artifacts")
    print(f"legacy probe SHA256: {old_digest}", flush=True)
    print(f"candidate probe SHA256: {new_digest}", flush=True)
    with tempfile.TemporaryDirectory(prefix="nulang-ai-local-cross-binary-") as root:
        work = Path(root)
        original, upgrade, restored = work / "legacy", work / "upgrade", work / "restored"
        backup = work / "immutable-agents.db"
        run(legacy, "seed", original)
        run(legacy, "check-old", original)
        source = original / "agents.db"
        integrity(source)
        with closing(sqlite3.connect(source)) as src, closing(sqlite3.connect(backup)) as dst:
            src.backup(dst)
            dst.commit()
        integrity(backup)
        backup.chmod(0o400)
        immutable = digest(backup)
        print(f"immutable backup SHA256: {immutable}", flush=True)

        upgrade.mkdir()
        shutil.copyfile(backup, upgrade / "agents.db")
        run(candidate, "upgrade", upgrade)
        run(candidate, "check-new", upgrade)
        integrity(upgrade / "agents.db")
        run(legacy, "check-old", original)

        restored.mkdir()
        shutil.copyfile(backup, restored / "agents.db")
        run(legacy, "check-old", restored)
        integrity(restored / "agents.db")
        if digest(backup) != immutable:
            raise RuntimeError("rehearsal mutated the immutable backup")
        print("PASS: AI-local old write/reopen, candidate update/reopen, legacy restored-backup rollback", flush=True)


if __name__ == "__main__":
    main()
