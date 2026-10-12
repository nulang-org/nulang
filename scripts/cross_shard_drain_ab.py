#!/usr/bin/env python3
"""Exact-base, same-host A/B for repeated cross-shard receive passes.

Identical example fixture is copied to the detached base checkout; the
runtime implementation is the only differing input. Results are diagnostic:
shared CI hosts and small samples do not justify broad throughput claims.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))
import confirm_bench_regression as shared

ROOT = SCRIPT_DIR.parent
BATCHES = (1, 4, 16)
RECORD = re.compile(
    r"\[cross-shard-drain\] batch=(?P<batch>\d+) "
    r"rounds=(?P<rounds>\d+) elapsed_ns=(?P<elapsed>\d+)"
)


def parse_measurement(output: str, batch: int, rounds: int) -> int:
    matches = list(RECORD.finditer(output))
    if len(matches) != 1:
        raise RuntimeError("missing or ambiguous cross-shard-drain measurement")
    record = matches[0]
    if int(record["batch"]) != batch or int(record["rounds"]) != rounds:
        raise RuntimeError("cross-shard benchmark workload mismatch")
    elapsed = int(record["elapsed"])
    if elapsed <= 0:
        raise RuntimeError("cross-shard benchmark elapsed_ns must be positive")
    return elapsed


def summarize(base: list[float], candidate: list[float]) -> dict:
    if not base or len(base) != len(candidate):
        raise ValueError("paired samples must have equal positive lengths")
    if any(not math.isfinite(n) or n <= 0 for n in base + candidate):
        raise ValueError("paired elapsed times must be finite and positive")
    speedups = [b / c for b, c in zip(base, candidate)]
    lower, upper = shared._bootstrap_median_ci(speedups)
    return {
        "pairs": len(base),
        "base_median_elapsed_ns": statistics.median(base),
        "candidate_median_elapsed_ns": statistics.median(candidate),
        "median_latency_change_pct": statistics.median(
            [(c / b - 1.0) * 100 for b, c in zip(base, candidate)]
        ),
        "median_speedup_x": statistics.median(speedups),
        "speedup_ci95_lower": lower,
        "speedup_ci95_upper": upper,
    }


def build_example(root: Path, env: dict[str, str]) -> None:
    shared.command_output(
        [
            "cargo", "build", "--locked", "--release",
            "--no-default-features", "--example", "cross_shard_drain",
        ],
        cwd=root,
        env=env,
    )


def compare(base_ref: str, runs: int, warmup: int, rounds: int) -> dict:
    shared.validate_run_count(runs)
    if warmup < 0 or rounds < 1:
        raise ValueError("warmup must be non-negative and rounds must be positive")
    fixture = ROOT / "examples" / "cross_shard_drain.rs"
    base = shared.add_worktree(base_ref)
    target_root = Path(tempfile.mkdtemp(prefix="nulang-shard-drain-ab-"))
    try:
        shutil.copyfile(fixture, base / "examples" / "cross_shard_drain.rs")
        repos = {"base": base, "candidate": ROOT}
        envs = {
            variant: shared.cargo_environment(target_root / variant)
            for variant in ("base", "candidate")
        }
        for variant in ("base", "candidate"):
            print(f"[compile] {variant}", flush=True)
            build_example(repos[variant], envs[variant])

        affinity = shared.measurement_affinity("single")
        measurements = {}
        for batch in BATCHES:
            samples: dict[str, list[float]] = {"base": [], "candidate": []}
            for round_idx in range(runs + warmup):
                for variant in shared.measurement_order(round_idx):
                    binary = target_root / variant / "release" / "examples" / "cross_shard_drain"
                    output = shared.command_output(
                        [str(binary), str(batch), str(rounds)],
                        cwd=repos[variant],
                        env=envs[variant],
                        cpu_affinity=affinity,
                    )
                    elapsed_ns = parse_measurement(output, batch, rounds)
                    if round_idx >= warmup:
                        samples[variant].append(float(elapsed_ns))
            measurements[str(batch)] = summarize(
                samples["base"], samples["candidate"]
            )
        return {
            "base_ref": base_ref,
            "fixture": "candidate example overlaid on exact-base worktree",
            "rounds_per_measurement": rounds,
            "paired_samples": runs,
            "warmup": warmup,
            "cpu_mode": "single",
            "results": measurements,
        }
    finally:
        shared.remove_worktree(base)
        shutil.rmtree(target_root, ignore_errors=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--runs", type=int, default=4)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--rounds-per-measurement", type=int, default=2000)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    data = compare(
        args.base_ref, args.runs, args.warmup, args.rounds_per_measurement
    )
    print("Exact-base cross-shard receive-pass A/B (negative latency delta is better)")
    for batch, result in data["results"].items():
        print(
            f"batch={batch} paired_latency_delta="
            f"{result['median_latency_change_pct']:+.2f}% "
            f"median_speedup={result['median_speedup_x']:.3f}x "
            f"95% CI=[{result['speedup_ci95_lower']:.3f}, "
            f"{result['speedup_ci95_upper']:.3f}]"
        )
    if args.output:
        args.output.write_text(json.dumps(data, indent=2) + "\n")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, OSError, subprocess.CalledProcessError) as exc:
        print(f"ERROR: paired cross-shard drain benchmark: {exc}", file=sys.stderr)
        sys.exit(1)
