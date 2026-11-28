#!/usr/bin/env python3
"""Rehearse WAL recovery after SIGKILL of a disposable Nulang probe.

The two probes must be built from the same fixture source at pinned old/new
revisions. No database path is accepted. All child processes and databases
are created by this harness; it never signals an unrelated process.
"""
import argparse
import os
import selectors
import shutil
import signal
import sqlite3
import subprocess
import tempfile
import time
from contextlib import closing
from pathlib import Path

from verify_sqlite_cross_binary import digest, integrity, run

READY = b"NULANG_WAL_CRASH_READY\n"


def hard_kill(probe: Path, db: Path) -> None:
    # The marker is emitted only after committing and verifying new state,
    # while the store remains open. Read unbuffered so a second line cannot
    # be stranded in Python's file-object buffer after select().
    deadline = time.monotonic() + 30
    output = bytearray()
    with subprocess.Popen(
        [str(probe), "upgrade-crash", str(db)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    ) as child:
        try:
            assert child.stdout is not None
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                while READY not in output:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not selector.select(remaining):
                        raise RuntimeError("writer did not acknowledge commit before deadline")
                    chunk = os.read(child.stdout.fileno(), 4096)
                    if not chunk:
                        raise RuntimeError(
                            f"writer exited before acknowledgment: {output.decode(errors='replace')}"
                        )
                    output.extend(chunk)
                    if len(output) > 65536:
                        raise RuntimeError("writer exceeded output budget")
            if child.poll() is not None:
                raise RuntimeError("writer exited normally before hard kill")
            child.kill()
            if child.wait(timeout=5) != -signal.SIGKILL:
                raise RuntimeError("writer did not terminate by SIGKILL")
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)


def wal_path(db: Path) -> Path:
    return db.with_name(db.name + "-wal")


def wal_frames(db: Path) -> int:
    wal = wal_path(db)
    with wal.open("rb") as stream:
        header = stream.read(32)
    if len(header) != 32 or int.from_bytes(header[:4], "big") not in (0x377F0682, 0x377F0683):
        raise RuntimeError("no valid WAL header after SIGKILL")
    page_size = int.from_bytes(header[8:12], "big")
    if page_size < 512 or page_size > 65536 or page_size & (page_size - 1):
        raise RuntimeError(f"invalid WAL page size: {page_size}")
    frames = (wal.stat().st_size - 32) // (page_size + 24)
    if frames < 1:
        raise RuntimeError("no complete WAL frames after SIGKILL")
    return frames


def recover(source: Path, destination: Path, reader: Path) -> None:
    # Writers are stopped. Preserve the main file AND WAL; SQLite recreates
    # the transient shared-memory WAL index on the disposable destination.
    shutil.copyfile(source, destination)
    shutil.copyfile(wal_path(source), wal_path(destination))
    run(reader, "check-new", destination)
    run(reader, "check-new", destination)  # independent clean reopen
    integrity(destination)


def main() -> None:
    if os.name != "posix":
        raise SystemExit("This SIGKILL rehearsal requires a POSIX host")
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
    with tempfile.TemporaryDirectory(prefix="nulang-sqlite-hard-kill-") as root:
        work = Path(root)
        original, backup = work / "seed.sqlite", work / "pre-upgrade.sqlite"
        run(legacy, "seed", original)
        run(legacy, "check-old", original)
        integrity(original)
        with closing(sqlite3.connect(original)) as src, closing(sqlite3.connect(backup)) as dst:
            src.backup(dst)
            dst.commit()
        integrity(backup)
        backup.chmod(0o400)
        backup_digest = digest(backup)

        for name, writer in (("legacy", legacy), ("candidate", candidate)):
            crashed = work / f"{name}-crashed.sqlite"
            shutil.copyfile(backup, crashed)
            hard_kill(writer, crashed)
            frames = wal_frames(crashed)
            before = (digest(crashed), digest(wal_path(crashed)))
            print(f"{name}: SIGKILL left {frames} complete WAL frames", flush=True)

            # Negative control on a throwaway main-only copy. This is NOT a
            # backup procedure: it proves the acknowledged update really was
            # absent from the main file and must be recovered from the WAL.
            main_only = work / f"{name}-main-only.sqlite"
            shutil.copyfile(crashed, main_only)
            run(writer, "check-old", main_only)

            if name == "legacy":
                recover(crashed, work / "legacy-recovered-by-legacy.sqlite", legacy)
            recover(crashed, work / f"{name}-recovered-by-candidate.sqlite", candidate)
            if (digest(crashed), digest(wal_path(crashed))) != before:
                raise RuntimeError("rehearsal mutated the stopped crashed source")
            if digest(backup) != backup_digest:
                raise RuntimeError("rehearsal changed the immutable backup")

        # Old writer sees the original state on a restored pre-upgrade
        # backup, not the candidate-upgraded database. Later writes are lost.
        restored = work / "restored.sqlite"
        shutil.copyfile(backup, restored)
        run(legacy, "check-old", restored)
        integrity(restored)
        if digest(backup) != backup_digest:
            raise RuntimeError("rollback changed the immutable backup")
        print("PASS: SIGKILL WAL recovery in both writers, cross-binary legacy WAL recovery, immutable backup rollback", flush=True)


if __name__ == "__main__":
    main()
