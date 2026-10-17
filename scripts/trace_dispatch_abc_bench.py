#!/usr/bin/env python3
"""Same-runner three-arm Nulang trace dispatch experiment.

Compare exact baseline, optimized, and eager-root control commits. Builds are
isolated and complete before timing. Every six-round cycle runs all six variant
permutations, counterbalancing execution position across the three revisions.
Diagnostic only: no automatic runtime promotion decision.
"""
from __future__ import annotations

import argparse
import itertools
import json
import os
import platform
import random
import re
import shutil
import statistics
import subprocess
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TARGET_ROOT = Path(tempfile.gettempdir()) / "nulang-trace-abc-cargo-target"
VARIANTS = ("base", "optimized", "eager_root")
PERMUTATIONS = tuple(itertools.permutations(VARIANTS))
TARGETS = (
    "ping_pong", "thread_ring", "fork_join",
    "ab/enqueue_payload_0", "ab/jit_warm_hot_loop_100k",
    "ab/mailbox_push_inline_1",
)
CROSS_RE = re.compile(
    r"\[cross-bench\]\s+runtime=nulang\s+"
    r"benchmark=(?P<name>[a-z0-9_-]+)\s+messages=(?P<count>\d+)\s+"
    r"elapsed_ns=(?P<elapsed>\d+)"
)
AB_RE = re.compile(
    r"\[ab-bench\]\s+benchmark=(?P<name>[a-z0-9_-]+)\s+"
    r"operations=(?P<count>\d+)\s+elapsed_ns=(?P<elapsed>\d+)"
)


def round_order(index: int) -> tuple[str, str, str]:
    return PERMUTATIONS[index % len(PERMUTATIONS)]


def require_sha(value: str) -> str:
    if not re.fullmatch(r"[a-fA-F0-9]{40}", value):
        raise ValueError("each variant must be an exact 40-character commit SHA")
    return value.lower()


def cargo_environment(variant: str) -> dict[str, str]:
    if variant not in VARIANTS:
        raise ValueError(f"unknown variant: {variant}")
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(TARGET_ROOT / variant)
    env["CARGO_INCREMENTAL"] = "0"
    return env


def parse_targets(output: str) -> dict[str, tuple[int, int]]:
    records = {}
    for pattern, prefix in ((CROSS_RE, ""), (AB_RE, "ab/")):
        for match in pattern.finditer(output):
            name = prefix + match.group("name")
            if name in TARGETS:
                if name in records:
                    raise RuntimeError(f"duplicate benchmark record: {name}")
                records[name] = (int(match.group("count")), int(match.group("elapsed")))
    missing = set(TARGETS) - set(records)
    if missing:
        raise RuntimeError(f"missing benchmark records: {sorted(missing)}")
    return records


def _bootstrap_ci(values: list[float], iterations: int = 4_000) -> tuple[float, float]:
    if not values:
        raise ValueError("bootstrap requires samples")
    if len(values) == 1:
        return values[0], values[0]
    rng = random.Random(0x4E554C41)
    n = len(values)
    medians = sorted(
        statistics.median(values[rng.randrange(n)] for _ in range(n))
        for _ in range(iterations)
    )
    return medians[int((iterations - 1) * 0.025)], medians[int((iterations - 1) * 0.975)]


def paired_results(samples: dict[str, dict[str, list[tuple[int, int]]]]) -> dict:
    if set(samples) != set(VARIANTS):
        raise RuntimeError("three-arm comparison must contain every variant")
    common = set.intersection(*(set(samples[v]) for v in VARIANTS))
    if not common:
        raise RuntimeError("no common workloads")
    results = {}
    for name in sorted(common):
        runs = [samples[v][name] for v in VARIANTS]
        if not all(len(rows) == len(runs[0]) for rows in runs):
            raise RuntimeError(f"{name}: paired round count differs between variants")
        if not runs[0]:
            raise RuntimeError(f"{name}: paired round count cannot be zero")
        for index, entries in enumerate(zip(*runs)):
            if len({count for count, _ in entries}) != 1:
                raise RuntimeError(f"{name}: operation count differs in round {index}")
            if any(elapsed <= 0 for _, elapsed in entries):
                raise RuntimeError(f"{name}: elapsed_ns must be positive")
        comparisons = {}
        for denominator, numerator, label in (
            ("base", "optimized", "optimized_vs_base"),
            ("base", "eager_root", "eager_root_vs_base"),
            ("eager_root", "optimized", "optimized_vs_eager_root"),
        ):
            speedups = [
                base_elapsed / candidate_elapsed
                for (_, base_elapsed), (_, candidate_elapsed)
                in zip(samples[denominator][name], samples[numerator][name])
            ]
            low, high = _bootstrap_ci(speedups)
            comparisons[label] = {
                "pairs": len(speedups),
                "median_speedup_x": statistics.median(speedups),
                "median_throughput_change_pct": statistics.median(
                    [(v - 1) * 100 for v in speedups]
                ),
                "speedup_ci95_lower": low,
                "speedup_ci95_upper": high,
            }
        results[name] = comparisons
    return results


def call(
    command: list[str], *, cwd: Path, env: dict | None = None,
    cpu: set[int] | None = None,
) -> str:
    def pin_cpu() -> None:
        if cpu is not None:
            os.sched_setaffinity(0, cpu)

    proc = subprocess.run(
        command, cwd=cwd, env=env, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        preexec_fn=pin_cpu if cpu is not None else None,
        check=False,
    )
    if proc.returncode:
        print(proc.stdout, flush=True)
        raise subprocess.CalledProcessError(
            proc.returncode, command, output=proc.stdout
        )
    return proc.stdout


def pinning(cpu_mode: str) -> set[int] | None:
    if cpu_mode == "host":
        return None
    if not hasattr(os, "sched_getaffinity") or not hasattr(os, "sched_setaffinity"):
        raise RuntimeError("single CPU mode requires Linux sched_setaffinity")
    allowed = sorted(os.sched_getaffinity(0))
    if not allowed:
        raise RuntimeError("empty process CPU affinity")
    return {allowed[0]}


def version(command: list[str]) -> str | None:
    try:
        return call(command, cwd=ROOT).strip()
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for variant in VARIANTS:
        parser.add_argument(f"--{variant.replace('_', '-')}-sha", required=True)
    parser.add_argument("--runs", type=int, default=6)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--cpu-mode", choices=("single", "host"), default="single")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.runs < 6 or args.runs % 6:
        parser.error("--runs must be a positive multiple of six for balanced positions")
    if args.warmup < 0:
        parser.error("--warmup must be nonnegative")
    refs = {v: require_sha(getattr(args, v + "_sha")) for v in VARIANTS}
    if len(set(refs.values())) != 3:
        parser.error("the three exact refs must be different")
    cpu = pinning(args.cpu_mode)
    worktrees: dict[str, Path] = {}
    root = Path(tempfile.mkdtemp(prefix="nulang-trace-abc-"))
    cargo_args = ["--no-default-features", "--features", "native-codegen"]
    try:
        for v in VARIANTS:
            dest = root / v
            print(f"[checkout] {v} {refs[v]}", flush=True)
            call(["git", "worktree", "add", "--detach", str(dest), refs[v]], cwd=ROOT)
            actual = call(["git", "rev-parse", "HEAD"], cwd=dest).strip()
            if actual != refs[v]:
                raise RuntimeError(f"ref mismatch: {v} resolved to {actual}")
            worktrees[v] = dest
        for v in VARIANTS:
            if "[cross-bench]" not in (
                worktrees[v] / "src" / "benchmarks.rs"
            ).read_text():
                raise RuntimeError(f"{v} benchmark records missing")
            print(f"[build] {v}", flush=True)
            call(
                ["cargo", "test", "--locked", "--release", "--no-run",
                 *cargo_args, "bench_"],
                cwd=worktrees[v], env=cargo_environment(v),
            )
        samples = {v: {name: [] for name in TARGETS} for v in VARIANTS}
        for round_idx in range(args.warmup + args.runs):
            measured = round_idx >= args.warmup
            print(
                f"[{'sample' if measured else 'warmup'} {round_idx + 1}] "
                f"order={round_order(round_idx)}", flush=True,
            )
            for v in round_order(round_idx):
                output = call(
                    ["cargo", "test", "--locked", "--release", *cargo_args,
                     "bench_", "--", "--nocapture", "--test-threads=1"],
                    cwd=worktrees[v], env=cargo_environment(v), cpu=cpu,
                )
                records = parse_targets(output)
                if measured:
                    for name, entry in records.items():
                        samples[v][name].append(entry)
        paired = paired_results(samples)
        for workload, comps in paired.items():
            for label, values in comps.items():
                print(
                    f"{workload} {label}: "
                    f"{values['median_throughput_change_pct']:+.2f}% "
                    f"CI [{values['speedup_ci95_lower']:.3f}, "
                    f"{values['speedup_ci95_upper']:.3f}]", flush=True,
                )
        report = {
            "schema": 1,
            "method": "three-arm same-CPU interleaved round-paired diagnostics",
            "created_at": datetime.now(timezone.utc).isoformat(),
            "refs": refs,
            "measured_runs": args.runs,
            "warmup_runs": args.warmup,
            "cpu_mode": args.cpu_mode,
            "cpu_affinity": sorted(cpu) if cpu is not None else None,
            "machine": platform.machine(),
            "rustc": version(["rustc", "--version"]),
            "cargo": version(["cargo", "--version"]),
            "samples": samples,
            "paired_comparison": paired,
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(f"wrote {args.output}", flush=True)
    finally:
        for v in reversed(VARIANTS):
            if v in worktrees:
                subprocess.run(
                    ["git", "worktree", "remove", "--force", str(worktrees[v])],
                    cwd=ROOT, check=False, stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
        shutil.rmtree(root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
