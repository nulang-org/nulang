#!/usr/bin/env python3
"""Exact-base, matched-fixture timer deadline Criterion A/B on one pinned CPU.

Use only for PRs that add the timer benchmark and modify no other benchmark
entry. Overlay the identical timer fixture on base; fail closed otherwise.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BENCH_ENTRY = Path("benches/bench_main.rs")
BENCH_SOURCE = Path("benches/timer_bench.rs")
BENCH_GROUP = "timer/next_deadline"
LANES = ("active_root", "cancelled_root")
COUNTS = (64, 1024, 16_384)


def expected_bench_entry(base: str) -> str:
    """Apply only the two precise timer benchmark registrations to base."""
    module = "mod scheduler_bench;\n"
    group = "    scheduler_bench::benches,\n"
    if base.count(module) != 1 or base.count(group) != 2:
        raise RuntimeError("expected exactly one module and two group insertion points")
    return (base.replace(module, module + "mod timer_bench;\n", 1)
            .replace(group, group + "    timer_bench::benches,\n"))


def matched_bench_entry(base: str, candidate: str) -> str:
    if expected_bench_entry(base) != candidate:
        raise RuntimeError("unrelated benchmark entry changes; refusing fixture overlay")
    return candidate


def read_estimates(target: Path) -> dict[str, float]:
    values: dict[str, float] = {}
    for lane in LANES:
        for count in COUNTS:
            key = f"{lane}/{count}"
            report = (target / "criterion" / "timer_next_deadline" / lane
                      / str(count) / "new" / "estimates.json")
            if not report.is_file():
                raise RuntimeError(f"missing Criterion estimate: {report}")
            value = float(json.loads(report.read_text())["mean"]["point_estimate"])
            if not math.isfinite(value) or value <= 0:
                raise RuntimeError(f"invalid Criterion mean for {key}: {value}")
            values[key] = value
    return values


def summarize_pairs(base: list[float], candidate: list[float]) -> dict[str, float | int]:
    if not base or len(base) != len(candidate):
        raise RuntimeError("unequal or empty paired samples")
    if any(not math.isfinite(v) or v <= 0 for v in base + candidate):
        raise RuntimeError("invalid paired timing (nonfinite or nonpositive)")
    return {
        "pairs": len(base),
        "median_base_ns": statistics.median(base),
        "median_candidate_ns": statistics.median(candidate),
        "median_speedup_x": statistics.median(b / c for b, c in zip(base, candidate)),
    }


def run(command: list[str], cwd: Path, *, env: dict[str, str] | None = None,
        cpu: int | None = None) -> str:
    def pin() -> None:
        if cpu is not None:
            os.sched_setaffinity(0, {cpu})

    result = subprocess.run(
        command, cwd=cwd, env=env, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        preexec_fn=pin if cpu is not None else None, check=False,
    )
    if result.returncode:
        sys.stderr.write(result.stdout)
        raise subprocess.CalledProcessError(result.returncode, command)
    return result.stdout


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True, help="exact PR base commit SHA")
    parser.add_argument("--runs", type=int, default=8)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--output", type=Path, default=Path("timer-deadline-ab.json"))
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.base_ref):
        parser.error("--base-ref must be an exact 40-character commit SHA")
    if args.runs < 2 or args.warmup < 0:
        parser.error("--runs must be >=2 and --warmup >=0")
    if not hasattr(os, "sched_getaffinity") or not hasattr(os, "sched_setaffinity"):
        raise RuntimeError("Linux CPU affinity is required")
    allowed = sorted(os.sched_getaffinity(0))
    if not allowed:
        raise RuntimeError("no permitted CPU found")
    cpu = allowed[0]

    root = Path(tempfile.mkdtemp(prefix="timer-deadline-ab-"))
    base = root / "base"
    attached = False
    try:
        run(["git", "worktree", "add", "--detach", str(base), args.base_ref], ROOT)
        attached = True
        candidate_entry = (ROOT / BENCH_ENTRY).read_text()
        base_entry_path = base / BENCH_ENTRY
        matched_bench_entry(base_entry_path.read_text(), candidate_entry)
        if (base / BENCH_SOURCE).exists():
            raise RuntimeError("timer benchmark already exists on base; fixture mismatch")
        fixture_bytes = (ROOT / BENCH_SOURCE).read_bytes()
        (base / BENCH_SOURCE).write_bytes(fixture_bytes)
        base_entry_path.write_text(candidate_entry)

        variants = {"base": base, "candidate": ROOT}
        environments: dict[str, dict[str, str]] = {}
        for name, directory in variants.items():
            env = os.environ.copy()
            env["CARGO_TARGET_DIR"] = str(root / f"target-{name}")
            environments[name] = env
            print(f"[build] {name}", flush=True)
            run(["cargo", "bench", "--locked", "--no-default-features",
                 "--bench", "bench_main", "--no-run"], directory, env=env)

        labels = [f"{lane}/{count}" for lane in LANES for count in COUNTS]
        samples = {name: {label: [] for label in labels} for name in variants}
        for i in range(args.runs + args.warmup):
            order = ("base", "candidate") if i % 2 == 0 else ("candidate", "base")
            for name in order:
                kind = "warmup" if i < args.warmup else "sample"
                print(f"[{kind} {i + 1}] {name}", flush=True)
                run(["cargo", "bench", "--locked", "--no-default-features",
                     "--bench", "bench_main", "--", BENCH_GROUP,
                     "--sample-size", "10", "--warm-up-time", "0.2",
                     "--measurement-time", "1", "--noplot"],
                    variants[name], env=environments[name], cpu=cpu)
                found = read_estimates(Path(environments[name]["CARGO_TARGET_DIR"]))
                if i >= args.warmup:
                    for label, measured in found.items():
                        samples[name][label].append(measured)

        comparison = {label: summarize_pairs(samples["base"][label],
                                            samples["candidate"][label]) for label in labels}
        report = {
            "schema": 1,
            "methodology": "exact-base matched fixture; single pinned CPU; paired Criterion means",
            "base_sha": run(["git", "rev-parse", "HEAD"], base).strip(),
            "candidate_sha": run(["git", "rev-parse", "HEAD"], ROOT).strip(),
            "fixture_sha256": hashlib.sha256(fixture_bytes).hexdigest(),
            "cpu": cpu, "warmup_rounds": args.warmup, "paired_rounds": args.runs,
            "samples_ns": samples, "comparison": comparison,
        }
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        for label, values in comparison.items():
            print(f"{label}: base {values['median_base_ns']:.1f} ns, "
                  f"candidate {values['median_candidate_ns']:.1f} ns, "
                  f"paired {values['median_speedup_x']:.3f}x", flush=True)
        return 0
    finally:
        if attached:
            subprocess.run(["git", "worktree", "remove", "--force", str(base)],
                           cwd=ROOT, check=False, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
