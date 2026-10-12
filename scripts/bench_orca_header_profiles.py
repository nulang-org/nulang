#!/usr/bin/env python3
"""Paired Criterion comparison of Nulang's default and compact ORCA headers.

Runs identical benchmark source on the SAME checked-out commit. The ABBA
order reduces first/last-run bias; it does not eliminate host contention or
substitute for statistically significant repeated measurements.

Usage:
  python3 scripts/bench_orca_header_profiles.py
  python3 scripts/bench_orca_header_profiles.py --cycles 2 --out target/orca-study
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
from statistics import mean
from typing import Any

FILTERS = ("gc/actor_heap_small_alloc", "gc/orca_local_alloc_drop")
PROFILES = ("default", "compact", "compact", "default")


def criterion_means(criterion_dir: Path) -> dict[str, float]:
    """Return mean nanoseconds keyed by stable Criterion benchmark ID."""
    values: dict[str, float] = {}
    for file in criterion_dir.rglob("new/estimates.json"):
        # The JSON is under criterion/<group>/<parameter>/new/estimates.json.
        name = file.parent.parent.relative_to(criterion_dir).as_posix()
        if not any(name.startswith(prefix + "/") for prefix in FILTERS):
            continue
        data = json.loads(file.read_text(encoding="utf-8"))
        estimate = data["mean"]["point_estimate"]
        if not isinstance(estimate, (int, float)) or estimate <= 0:
            raise ValueError(f"Invalid mean estimate in {file}")
        values[name] = float(estimate)
    if not values:
        raise RuntimeError(f"No Criterion estimates found in {criterion_dir}")
    missing = [
        prefix for prefix in FILTERS
        if not any(name.startswith(prefix + "/") for name in values)
    ]
    if missing:
        raise RuntimeError(f"Missing Criterion groups in {criterion_dir}: {missing}")
    return values


def summarize(samples: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Compute per-benchmark means and signed latency changes, no speedup claims."""
    by_name: dict[str, dict[str, list[float]]] = {}
    for sample in samples:
        profile = sample["profile"]
        for name, latency in sample["means_ns"].items():
            by_name.setdefault(name, {"default": [], "compact": []})[profile].append(
                float(latency)
            )
    rows: list[dict[str, Any]] = []
    for name, profiles in sorted(by_name.items()):
        if not profiles["default"] or not profiles["compact"]:
            raise ValueError(f"Missing one profile for {name}")
        default_ns = mean(profiles["default"])
        compact_ns = mean(profiles["compact"])
        rows.append(
            {
                "benchmark": name,
                "default_mean_ns": default_ns,
                "compact_mean_ns": compact_ns,
                "latency_change_pct": 100.0 * (compact_ns / default_ns - 1.0),
                "default_samples": len(profiles["default"]),
                "compact_samples": len(profiles["compact"]),
            }
        )
    return rows


def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], text=True).strip()


def run(out: Path, cycles: int, allow_dirty: bool) -> None:
    root = Path(__file__).resolve().parent.parent
    os.chdir(root)
    commit = git("rev-parse", "HEAD")
    if not allow_dirty and git("status", "--porcelain"):
        raise SystemExit(
            "Refusing to benchmark a dirty worktree; commit your changes "
            "or use --allow-dirty (results will record that override)."
        )
    out = out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    samples: list[dict[str, Any]] = []

    for cycle in range(cycles):
        for step, profile in enumerate(PROFILES):
            features = ["--features", "compact-orca-header"] if profile == "compact" else []
            target_dir = out / "build" / profile
            criterion_dir = target_dir / "criterion"
            # Preserve compilation caches but never read a previous run's
            # Criterion estimates if the current benchmark did not execute.
            if criterion_dir.exists():
                shutil.rmtree(criterion_dir)
            env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir))
            for benchmark_filter in FILTERS:
                command = [
                    "cargo", "bench", "--locked", "--bench", "bench_main",
                    *features, "--", benchmark_filter,
                ]
                print(f"cycle={cycle + 1} step={step + 1} profile={profile}: "
                      + " ".join(command), flush=True)
                subprocess.run(command, env=env, check=True)
            estimates = criterion_means(criterion_dir)
            sample = {
                "cycle": cycle + 1,
                "step": step + 1,
                "profile": profile,
                "means_ns": estimates,
            }
            samples.append(sample)
            (out / f"cycle-{cycle + 1}-step-{step + 1}-{profile}.json").write_text(
                json.dumps(sample, indent=2) + "\n", encoding="utf-8"
            )

    report = {
        "commit": commit,
        "dirty_worktree_allowed": allow_dirty,
        "os": platform.platform(),
        "python": sys.version.split()[0],
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "cycles": cycles,
        "filters": FILTERS,
        "order": PROFILES,
        "samples": samples,
        "summary": summarize(samples),
        "disclaimer": (
            "Criterion nanosecond estimates; no RSS measurement. "
            "Signed latency change is descriptive, not proof of improvement."
        ),
    }
    (out / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"Saved {out / 'report.json'}")
    for row in report["summary"]:
        print(
            f"{row['benchmark']}: default={row['default_mean_ns']:.2f} ns, "
            f"compact={row['compact_mean_ns']:.2f} ns, "
            f"latency change={row['latency_change_pct']:+.2f}%"
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cycles", type=int, default=1, help="ABBA cycles (minimum 1)")
    parser.add_argument(
        "--out", type=Path, default=Path("target/orca-header-profile-study")
    )
    parser.add_argument("--allow-dirty", action="store_true")
    args = parser.parse_args()
    if args.cycles < 1:
        parser.error("--cycles must be >= 1")
    run(args.out, args.cycles, args.allow_dirty)


if __name__ == "__main__":
    main()
