#!/usr/bin/env python3
"""Crash/restart conformance comparison; executes each phase as a new OS process.

No network access, admin access, or services are required. The single-job test
verifies state continuity; it DOES NOT prove exactly-once external side effects.
"""
import argparse
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent


def run_phase(command: list[str], cwd: Path, phase: str, expected: list[str], timeout: float) -> None:
    env = os.environ.copy()
    env["DEMO_PHASE"] = phase
    env["DEMO_BASELINE_DB"] = str(cwd / "baseline.sqlite3")
    if phase == "prepare":
        proc = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                encoding="utf-8", bufsize=1)
        try:
            assert proc.stdout is not None
            observed: list[str] = []
            deadline = time.monotonic() + timeout
            while not all(item in observed for item in expected):
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError(f"prepare timed out, output={observed}")
                ready, _, _ = select.select([proc.stdout], [], [], remaining)
                if not ready:
                    raise RuntimeError(f"prepare did not emit checkpoint, output={observed}")
                line = proc.stdout.readline()
                if not line:
                    out, err = proc.communicate(timeout=timeout)
                    raise RuntimeError(f"prepare exited before checkpoint: rc={proc.returncode}, "
                                       f"output={observed + out.splitlines()}, stderr={err.strip()}")
                observed.append(line.strip())
            if proc.poll() is not None:
                raise RuntimeError("prepare process exited before injection")
            # No graceful shutdown: simulate failure after the acknowledged
            # durable transition and before the next operation.
            proc.kill()
            out, err = proc.communicate(timeout=timeout)
            if proc.returncode != -signal.SIGKILL:
                raise RuntimeError(f"SIGKILL not confirmed: rc={proc.returncode}, stderr={err}")
            print(f"  killed after {expected[0]}")
        finally:
            if proc.poll() is None:
                proc.kill()
            proc.communicate()
    else:
        result = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True, timeout=timeout)
        if result.returncode != 0:
            raise RuntimeError(f"resume failed: rc={result.returncode}\nstdout={result.stdout}\nstderr={result.stderr}")
        lines = [line.strip() for line in result.stdout.splitlines()]
        missing = [item for item in expected if item not in lines]
        if missing:
            raise RuntimeError(f"expected {missing}; observed={lines}; stderr={result.stderr}")
        print("  " + ", ".join(expected))


def exercise(name: str, command: list[str], timeout: float) -> None:
    with tempfile.TemporaryDirectory(prefix=f"nulang-{name}-") as temp:
        wd = Path(temp)
        if name == "nulang":
            command = [command[0], "--backend", "bytecode", "--store", f"json:{wd / 'store'}", str(ROOT / "job.nula")]
        print(f"{name}: isolated store and three separate processes")
        run_phase(command, wd, "prepare", ["PREPARED=1:0"], timeout)
        run_phase(command, wd, "resume", ["RECOVERED=1:0", "COMPLETED=2:1"], timeout)
        run_phase(command, wd, "resume", ["RECOVERED=2:1", "COMPLETED=2:1"], timeout)
        print(f"PASS {name}: committed state recovered and completion remained idempotent")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nulang-bin", default="nulang", help="Full-featured Nulang CLI executable")
    parser.add_argument("--baseline-only", action="store_true", help="Verify only the Python/SQLite comparator")
    parser.add_argument("--timeout", type=float, default=20, help="Per-process timeout in seconds")
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("requires POSIX SIGKILL (Linux/macOS)")
    if args.timeout <= 0:
        parser.error("timeout must be positive")
    baseline = [sys.executable, str(ROOT / "sqlite_baseline.py")]
    exercise("sqlite", baseline, args.timeout)
    if not args.baseline_only:
        binary = shutil.which(args.nulang_bin)
        if binary is None:
            parser.error(f"Nulang executable not found: {args.nulang_bin}; pass --nulang-bin")
        exercise("nulang", [str(Path(binary).resolve())], args.timeout)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
