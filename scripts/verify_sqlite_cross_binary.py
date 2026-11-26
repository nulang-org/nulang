#!/usr/bin/env python3
"""Rehearse local SQLite migration and restored-backup rollback with two binaries.

Both are independently compiled from the same fixture source and distinct pinned
Nulang revisions. Never pass a production database: this script creates every
database itself in a private temporary directory.
"""
import argparse
import hashlib
import shutil
import sqlite3
import subprocess
import tempfile
from contextlib import closing
from pathlib import Path

def digest(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()

def run(probe: Path, command: str, db: Path) -> None:
    subprocess.run([str(probe), command, str(db)], check=True, timeout=30)

def integrity(path: Path) -> None:
    with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as db:
        result = db.execute("PRAGMA integrity_check").fetchall()
        if result != [("ok",)]:
            raise RuntimeError(f"integrity check failed for fixture {path.name}: {result!r}")

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--legacy-probe", required=True, type=Path)
    parser.add_argument("--candidate-probe", required=True, type=Path)
    args = parser.parse_args()
    legacy, candidate = args.legacy_probe.resolve(strict=True), args.candidate_probe.resolve(strict=True)
    if legacy == candidate or digest(legacy) == digest(candidate):
        raise SystemExit("Refusing identical old/new binary artifacts")
    print(f"legacy probe SHA256: {digest(legacy)}", flush=True)
    print(f"candidate probe SHA256: {digest(candidate)}", flush=True)
    with tempfile.TemporaryDirectory(prefix="nulang-sqlite-cross-binary-") as root:
        work = Path(root)
        original, backup = work / "legacy.sqlite", work / "pre-upgrade.sqlite"
        upgrade, restored = work / "upgrade.sqlite", work / "restored.sqlite"
        run(legacy, "seed", original)
        run(legacy, "check-old", original)
        integrity(original)
        # The old writer has exited. The SQLite backup API creates a
        # consistent copy instead of copying a possibly live WAL database.
        with closing(sqlite3.connect(original)) as src, closing(sqlite3.connect(backup)) as dst:
            src.backup(dst)
            dst.commit()
        integrity(backup)
        backup.chmod(0o400)
        immutable_digest = digest(backup)
        print(f"immutable pre-upgrade backup SHA256: {immutable_digest}", flush=True)
        shutil.copyfile(backup, upgrade)
        run(candidate, "upgrade", upgrade)
        run(candidate, "check-new", upgrade)
        integrity(upgrade)
        if digest(backup) != immutable_digest:
            raise RuntimeError("candidate changed the immutable backup")
        # Rehearse rollback AFTER the upgrade from the original backup, not
        # from upgraded files; the legacy binary must see the old state.
        shutil.copyfile(backup, restored)
        run(legacy, "check-old", restored)
        integrity(restored)
        if digest(backup) != immutable_digest:
            raise RuntimeError("rollback changed the immutable backup")
        print("PASS: old write/reopen, candidate upgrade/reopen, old binary restored-backup rollback", flush=True)

if __name__ == "__main__":
    main()
