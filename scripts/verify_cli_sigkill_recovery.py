#!/usr/bin/env python3
"""Verify committed state survives SIGKILL and a fresh Nulang CLI process.

The first CLI process stays alive in an explicit first-run loop after
confirming durable actor operations. The harness sends SIGKILL only after observing that marker and a
persisted snapshot. This is not a claim about arbitrary write-time crash windows.
"""
import argparse
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import sys
import tempfile
import time

SOURCE = '''persistent actor DurableCounter {
    state durable value: Int = 0
    behavior increment() {
        self.value = self.value + 1
        self.value
    }
    behavior current() { self.value }
}

fn main() {
    let c = spawn DurableCounter {} in {
        let before = ask c current()
        let ignored = ask c increment()
        let after = ask c current()
        perform IO.print("RESTORED=" + perform Int.to_string(before) + " SAVED=" + perform Int.to_string(after))
        // Keep the initial process live without relying on IO.read, which
        // the native CLI currently rejects as an unhandled effect.
        // On a fresh second process the restored counter is 1 and this
        // loop is skipped so restart verification can terminate.
        if before == 0 then {
            while true { unit }
        } else { unit }
    }
}
'''

class RecoveryError(Exception):
    """A CLI SIGKILL recovery contract failed."""


def parse_ready(line: str) -> tuple[int, int]:
    match = re.fullmatch(r"RESTORED=([0-9]+) SAVED=([0-9]+)", line.strip())
    if match is None:
        raise RecoveryError(f"invalid CLI readiness marker: {line!r}")
    return int(match.group(1)), int(match.group(2))


def read_ready(proc: subprocess.Popen, timeout: float) -> tuple[int, int]:
    """Wait for a newline-terminated marker without blocking indefinitely."""
    assert proc.stdout is not None
    deadline = time.monotonic() + timeout
    pending = b""
    observed = []
    with selectors.DefaultSelector() as selector:
        selector.register(proc.stdout, selectors.EVENT_READ)
        while (remaining := deadline - time.monotonic()) > 0:
            events = selector.select(timeout=min(0.25, remaining))
            if not events:
                if proc.poll() is not None:
                    raise RecoveryError(f"CLI exited before checkpoint: exit={proc.returncode}, output={observed!r}")
                continue
            chunk = os.read(proc.stdout.fileno(), 65536)
            if not chunk:
                raise RecoveryError(f"CLI closed stdout before checkpoint: {observed!r}")
            pending += chunk
            if len(pending) > 65536:
                raise RecoveryError("CLI output exceeds readiness budget")
            while b"\n" in pending:
                part, pending = pending.split(b"\n", 1)
                line = part.decode("utf-8", "replace").strip()
                observed.append(line)
                if line.startswith("RESTORED="):
                    return parse_ready(line)
    raise RecoveryError(f"timed out waiting for CLI checkpoint marker; observed={observed!r}")


def verify(binary: Path, timeout: float = 40) -> None:
    if os.name != "posix" or not hasattr(signal, "SIGKILL"):
        raise RecoveryError("requires a POSIX runner with SIGKILL")
    binary = Path(binary).resolve(strict=True)
    env = dict(os.environ, NULANG_SHARDS="1")
    with tempfile.TemporaryDirectory(prefix="nulang-sigkill-recovery-") as directory:
        root = Path(directory)
        source = root / "counter.nula"
        store = root / "store"
        source.write_text(SOURCE, encoding="utf-8")
        command = [str(binary), "--store", f"json:{store}", str(source)]
        with (root / "first-process-stderr.log").open("w+") as stderr:
            proc = subprocess.Popen(
                command, cwd=root, env=env,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=stderr, bufsize=0,
            )
            try:
                first = read_ready(proc, timeout)
                if first != (0, 1):
                    raise RecoveryError(f"fresh store expected (0, 1), observed {first}")
                snapshots = list(store.glob("actor_*/snapshot.json"))
                if not snapshots or not any(p.stat().st_size > 0 for p in snapshots):
                    raise RecoveryError("no nonempty durable actor snapshot on disk before SIGKILL")
                if proc.poll() is not None:
                    raise RecoveryError("process exited before SIGKILL; did not test live termination")
                proc.kill()
                exit_code = proc.wait(timeout=timeout)
                if exit_code != -signal.SIGKILL:
                    raise RecoveryError(f"expected SIGKILL termination, exit={exit_code}")
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=5)
                if proc.stdin:
                    proc.stdin.close()
                if proc.stdout:
                    proc.stdout.close()

        try:
            result = subprocess.run(
                command, cwd=root, env=env, input="\n", capture_output=True,
                text=True, timeout=timeout, check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise RecoveryError("second CLI process did not complete") from exc
        if result.returncode != 0:
            raise RecoveryError(f"second CLI exited {result.returncode}: stdout={result.stdout!r} stderr={result.stderr!r}")
        markers = [line for line in result.stdout.splitlines() if line.startswith("RESTORED=")]
        if len(markers) != 1:
            raise RecoveryError(f"second process expected one checkpoint marker: {result.stdout!r}")
        second = parse_ready(markers[0])
        if second != (1, 2):
            raise RecoveryError(f"state lost across SIGKILL: expected (1, 2), got {second}")
    print("PASS: committed value 1 survived SIGKILL; fresh CLI process restored 1 and wrote 2")
    print("Scope: single actor, JSON store, Linux/POSIX process kill after acknowledged write")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=40)
    args = parser.parse_args()
    try:
        verify(args.binary, timeout=args.timeout)
    except (RecoveryError, OSError, subprocess.TimeoutExpired) as exc:
        print(f"FAIL: SIGKILL recovery: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
