#!/usr/bin/env python3
"""Measure startup-to-exit latency, CPU time, and peak RSS for native ELF programs.

This is a small, opt-in, Linux-only measurement companion to the binary-size
smoke. It is not a microbenchmark or a guarantee of cold-cache start time.
Each sample creates a fresh process using fork/exec and reaps it with wait4.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import tempfile
import platform
import statistics
import sys
from pathlib import Path


MEASUREMENT_CONTRACT = (
    "Wall time covers native C fork/exec/wait4 and process exit, "
    "not just application initialization. Page cache is not cleared; "
    "stdout/stderr go to /dev/null. CPU time and peak RSS are per child "
    "from Linux wait4 (ru_maxrss in KiB). Compare only paired, same-host "
    "workloads; shared-runner timings are informational."
)


def profile_executable(
    binary: Path, *, runs: int = 21, warmup: int = 3, timeout_seconds: float = 30.0
) -> dict[str, int | str]:
    """Profile one executable; reject invalid paths and nonzero exits."""
    if not 1 <= runs <= 2000:
        raise ValueError("runs must be between 1 and 2000")
    if not 0 <= warmup <= 2000:
        raise ValueError("warmup must be between 0 and 2000")
    if not 0 < timeout_seconds <= 600:
        raise ValueError("timeout must be greater than 0 and at most 600 seconds")
    if sys.platform != "linux":
        raise RuntimeError("native runtime resource profiling requires Linux")
    binary = binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"not executable: {binary}")

    source = Path(__file__).with_name("native_runtime_probe.c")
    with tempfile.TemporaryDirectory(prefix="nulang-probe-") as tmp:
        probe = Path(tmp) / "probe"
        compiled = subprocess.run(
            ["cc", "-O2", "-Wall", "-Wextra", "-o", str(probe), str(source)],
            capture_output=True, text=True,
        )
        if compiled.returncode:
            raise RuntimeError(f"cannot compile native probe: {compiled.stderr[-2000:]}")
        # Start a separate process group so a stuck benchmark cannot leave
        # either the probe or its fork/exec descendants alive in CI.
        process = subprocess.Popen(
            [str(probe), str(binary), str(runs), str(warmup)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=timeout_seconds)
        except subprocess.TimeoutExpired as exc:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate()  # Reap the probe, close pipes.
            raise RuntimeError(f"native probe timed out after {timeout_seconds:g}s: {binary}") from exc
        if process.returncode:
            raise RuntimeError(f"native probe: {stderr[-2000:]}")
    rows = stdout.splitlines()
    if len(rows) != runs:
        raise RuntimeError(f"native probe returned {len(rows)} samples; expected {runs}")
    try:
        samples = [[int(field) for field in row.split("\t")] for row in rows]
    except ValueError as exc:
        raise RuntimeError(f"malformed native probe output: {exc}") from exc
    if any(len(row) != 3 or min(row) < 0 for row in samples):
        raise RuntimeError("invalid native probe measurements")
    wall_samples = [sample[0] for sample in samples]
    cpu_samples = [sample[1] for sample in samples]
    rss_samples = [sample[2] for sample in samples]
    wall_median = round(statistics.median(wall_samples))
    return {
        "runs": runs,
        "warmup": warmup,
        "median_wall_ns": wall_median,
        "min_wall_ns": min(wall_samples),
        "mad_wall_ns": round(statistics.median(abs(v - wall_median) for v in wall_samples)),
        "median_cpu_ns": round(statistics.median(cpu_samples)),
        "median_peak_rss_kib": round(statistics.median(rss_samples)),
        "measurement_contract": MEASUREMENT_CONTRACT,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("programs", nargs="+", metavar="NAME=EXECUTABLE",
                        help="one or more named native binaries")
    parser.add_argument("--runs", type=int, default=21)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument("--timeout-seconds", type=float, default=30.0,
                        help="maximum total measurement time per artifact (default: 30)")
    parser.add_argument("--json-out", type=Path)
    opts = parser.parse_args()

    try:
        artifacts = []
        seen = set()
        for value in opts.programs:
            name, sep, raw_path = value.partition("=")
            if not sep or not name or not raw_path or name in seen:
                raise ValueError(f"expected unique NAME=EXECUTABLE, got {value!r}")
            seen.add(name)
            binary = Path(raw_path).resolve()
            artifacts.append({"name": name, "executable": str(binary),
                              **profile_executable(binary, runs=opts.runs, warmup=opts.warmup,
                                                   timeout_seconds=opts.timeout_seconds)})
        report = {
            "schema_version": 1,
            "platform": {"system": platform.system(), "architecture": platform.machine()},
            "measurement_contract": MEASUREMENT_CONTRACT,
            "artifacts": artifacts,
        }
        encoded = json.dumps(report, indent=2)
        if opts.json_out is not None:
            opts.json_out.write_text(encoded + "\n", encoding="utf-8")
        print(encoded)
        return 0
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
