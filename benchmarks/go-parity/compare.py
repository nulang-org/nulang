#!/usr/bin/env python3
"""Run Nulang/Go parity benchmarks and print comparable ns/op ratios."""

from __future__ import annotations

import argparse
import json
import re
import statistics
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GO_DIR = ROOT / "benchmarks" / "go-parity"

GO_NAMES = {
    "int_loop": "BenchmarkIntLoop",
    "float_loop": "BenchmarkFloatLoop",
    "direct_call_loop": "BenchmarkDirectCallLoop",
    "fib25": "BenchmarkFib25",
}


def run(cmd: list[str], cwd: Path = ROOT) -> str:
    print("+", " ".join(cmd), flush=True)
    proc = subprocess.run(cmd, cwd=cwd, text=True, capture_output=True)
    if proc.returncode != 0:
        if proc.stdout:
            print(proc.stdout)
        if proc.stderr:
            print(proc.stderr)
        raise SystemExit(proc.returncode)
    return proc.stdout


def cargo_target_dir() -> Path:
    raw = run(["cargo", "metadata", "--no-deps", "--format-version=1"])
    return Path(json.loads(raw)["target_directory"])


def criterion_results(target: Path) -> dict[str, float]:
    root = target / "criterion"
    found: dict[str, float] = {}
    for meta in root.rglob("benchmark.json"):
        try:
            info = json.loads(meta.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        full_id = info.get("full_id", "")
        if not full_id.startswith("go_parity/"):
            continue
        estimates = meta.parent / "new" / "estimates.json"
        if not estimates.exists():
            continue
        data = json.loads(estimates.read_text())
        # Criterion stores duration estimates in nanoseconds.
        found[full_id] = float(data["mean"]["point_estimate"])
    return found


GO_LINE = re.compile(
    r"^(Benchmark(?:IntLoop|FloatLoop|DirectCallLoop|Fib25))-\d+\s+\d+\s+([0-9.]+)\s+ns/op"
)


def go_results(output: str) -> dict[str, float]:
    samples: dict[str, list[float]] = {}
    for line in output.splitlines():
        m = GO_LINE.match(line.strip())
        if m:
            samples.setdefault(m.group(1), []).append(float(m.group(2)))
    return {name: statistics.median(values) for name, values in samples.items()}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--no-run",
        action="store_true",
        help="read existing Criterion data and only run the Go benchmarks",
    )
    parser.add_argument("--go-count", type=int, default=5)
    args = parser.parse_args()

    target = cargo_target_dir()
    if not args.no_run:
        run(["cargo", "bench", "--bench", "bench_main", "--", "go_parity"])

    go_out = run(
        [
            "go",
            "test",
            "-run=^$",
            "-bench=.",
            "-benchmem",
            f"-count={args.go_count}",
        ],
        cwd=GO_DIR,
    )

    nu = criterion_results(target)
    go = go_results(go_out)
    if not nu:
        raise SystemExit("no go_parity Criterion results found")
    if not go:
        raise SystemExit("no Go benchmark results parsed")

    print()
    print(f"{'workload':<20} {'backend':<6} {'Nulang ns/op':>14} {'Go ns/op':>12} {'Nulang/Go':>11}")
    print("-" * 70)
    for workload, go_name in GO_NAMES.items():
        go_ns = go.get(go_name)
        if go_ns is None:
            continue
        for backend in ("jit", "aot"):
            key = f"go_parity/{backend}/{workload}"
            nu_ns = nu.get(key)
            if nu_ns is None:
                continue
            ratio = nu_ns / go_ns
            print(f"{workload:<20} {backend:<6} {nu_ns:>14.1f} {go_ns:>12.1f} {ratio:>10.2f}x")

    print()
    print("Ratio < 1.0 means Nulang is faster for that kernel; > 1.0 means Go is faster.")
    print("Compare on the same machine, power mode, CPU affinity, and toolchain build.")


if __name__ == "__main__":
    main()
