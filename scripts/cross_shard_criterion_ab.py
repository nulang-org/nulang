#!/usr/bin/env python3
"""Compare the same cross-shard Criterion fixture at exact PR base and head.

The fixture is intentionally copied from the candidate into the detached base
worktree. This makes the benchmark code identical on both sides even though
the base commit predates the new Criterion cases. Only Runtime differs.

This is diagnostic, not a CI regression gate: the shared runner can be noisy.
"""

from __future__ import annotations

import argparse
import json
import math
import shutil
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

# When unittest imports this script through importlib, its directory is not
# automatically added to sys.path. Keep the neighboring benchmark helper
# import working both as a CLI and as an imported test subject.
SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import confirm_bench_regression as shared

ROOT = SCRIPT_DIR.parent
BENCHMARKS = tuple(f"actor/cross_shard_enqueue_by_id/{n}" for n in (0, 1, 4, 5, 16))
GROUP = "actor/cross_shard_enqueue_by_id"


def bench_command() -> list[str]:
    return [
        "cargo", "bench", "--locked", "--bench", "bench_main",
        "--no-default-features", "--features", "native-codegen",
        "--", GROUP, "--noplot", "--color", "never",
        "--sample-size", "15", "--warm-up-time", "0.5",
        "--measurement-time", "1",
    ]


def build_command() -> list[str]:
    return [
        "cargo", "bench", "--locked", "--bench", "bench_main",
        "--no-default-features", "--features", "native-codegen", "--no-run",
    ]


def read_means(criterion_dir: Path) -> dict[str, float]:
    rows = shared.collect_means(criterion_dir)
    means: dict[str, float] = {}
    missing: list[str] = []
    for name in BENCHMARKS:
        group, parameter = name.rsplit("/", 1)
        # Criterion 0.5 sanitizes slash characters inside a *group ID* to
        # underscores when constructing paths, but retains the '/' separator
        # before its parameter. Benchmark display names remain unchanged.
        directory_name = f"{group.replace('/', '_')}/{parameter}"
        candidates = dict.fromkeys((name, directory_name))
        found = [key for key in candidates if key in rows]
        if len(found) > 1:
            raise RuntimeError(f"ambiguous Criterion output for {name}: {found}")
        if not found:
            missing.append(name)
            continue
        means[name] = rows[found[0]]
    if missing:
        raise RuntimeError(
            "missing Criterion results for " + ", ".join(missing)
            + "; available result paths: " + repr(sorted(rows))
        )
    if any(not math.isfinite(value) or value <= 0 for value in means.values()):
        raise RuntimeError("Criterion means must be positive and finite")
    return means


def summarize_pairs(base: list[float], candidate: list[float]) -> dict[str, float | int]:
    if not base or len(base) != len(candidate):
        raise ValueError("paired sample counts must be equal and nonempty")
    if any(not math.isfinite(x) or x <= 0 for x in base + candidate):
        raise ValueError("paired samples must be finite and positive")
    speedups = [b / c for b, c in zip(base, candidate)]
    latency_changes = [(c / b - 1.0) * 100.0 for b, c in zip(base, candidate)]
    lower, upper = shared._bootstrap_median_ci(speedups)
    return {
        "pairs": len(base),
        "base_median_ns": statistics.median(base),
        "candidate_median_ns": statistics.median(candidate),
        "median_speedup_x": statistics.median(speedups),
        "median_latency_change_pct": statistics.median(latency_changes),
        "speedup_ci95_lower": lower,
        "speedup_ci95_upper": upper,
    }


def run_benchmark(
    repo: Path, env: dict[str, str], target_dir: Path, affinity: set[int] | None
) -> dict[str, float]:
    shutil.rmtree(target_dir / "criterion", ignore_errors=True)
    shared.command_output(bench_command(), cwd=repo, env=env, cpu_affinity=affinity)
    return read_means(target_dir / "criterion")


def run_comparison(
    base_ref: str, runs: int, warmup: int, cpu_mode: str
) -> dict[str, object]:
    shared.validate_run_count(runs)
    if warmup < 0:
        raise ValueError("warmup must be >= 0")
    candidate_bench = ROOT / "benches" / "actor_bench.rs"
    if GROUP not in candidate_bench.read_text():
        raise RuntimeError(f"candidate benchmark does not contain {GROUP}")

    base = shared.add_worktree(base_ref)
    target_root = Path(tempfile.mkdtemp(prefix="nulang-cross-shard-criterion-"))
    try:
        # Critical apples-to-apples control: benchmark scenario code is EXACTLY
        # the same in candidate and base, even though base predates this group.
        shutil.copyfile(candidate_bench, base / "benches" / "actor_bench.rs")
        envs = {
            variant: shared.cargo_environment(target_root / variant)
            for variant in ("base", "candidate")
        }
        roots = {"base": base, "candidate": ROOT}
        for variant in ("base", "candidate"):
            print(f"[build] {variant}", flush=True)
            shared.command_output(build_command(), cwd=roots[variant], env=envs[variant])

        affinity = shared.measurement_affinity(cpu_mode)
        samples: dict[str, dict[str, list[float]]] = {
            variant: {name: [] for name in BENCHMARKS}
            for variant in ("base", "candidate")
        }
        for round_idx in range(runs + warmup):
            for variant in shared.measurement_order(round_idx):
                label = "warmup" if round_idx < warmup else "sample"
                print(f"[{label} {round_idx + 1}] {variant}", flush=True)
                measurements = run_benchmark(
                    roots[variant], envs[variant], target_root / variant, affinity
                )
                if round_idx >= warmup:
                    for name, value in measurements.items():
                        samples[variant][name].append(value)

        comparison = {
            name: summarize_pairs(samples["base"][name], samples["candidate"][name])
            for name in BENCHMARKS
        }
        return {
            "base_ref": base_ref,
            "benchmarks": list(BENCHMARKS),
            "comparison": comparison,
            "fixture": "candidate benches/actor_bench.rs copied to exact base",
            "profile": "no-default-features,native-codegen",
            "cpu_mode": cpu_mode,
            "runs": runs,
            "warmup": warmup,
        }
    finally:
        shared.remove_worktree(base)
        shutil.rmtree(target_root, ignore_errors=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--runs", type=int, default=4)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--cpu-mode", choices=("single", "host"), default="single")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    data = run_comparison(args.base_ref, args.runs, args.warmup, args.cpu_mode)
    print()
    print("Cross-shard Criterion exact-base paired A/B (lower latency is better)")
    print("payload   base ns/send   candidate ns/send   paired latency change   speedup 95% CI")
    for name, stats in data["comparison"].items():
        payload = name.rsplit("/", 1)[-1]
        print(
            f"{payload:>7}   {stats['base_median_ns']:>12.1f}"
            f"   {stats['candidate_median_ns']:>17.1f}"
            f"   {stats['median_latency_change_pct']:>+20.2f}%"
            f"   [{stats['speedup_ci95_lower']:.3f}, {stats['speedup_ci95_upper']:.3f}]"
        )
    if args.output:
        args.output.write_text(json.dumps(data, indent=2) + "\n")
    print("Diagnostic only: compare CI confidence intervals and repeat on a stable host.")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.CalledProcessError) as exc:
        print(f"ERROR: cross-shard paired Criterion comparison failed: {exc}", file=sys.stderr)
        sys.exit(1)
