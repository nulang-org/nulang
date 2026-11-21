#!/usr/bin/env python3
"""Confirm historical Criterion alerts with same-host parent-vs-candidate A/B.

The rolling history gate is intentionally sensitive to large shared-runner
shifts. When it fires, this script rebuilds the exact base ref and current
candidate in isolated Cargo target directories, then alternates filtered
Criterion runs on one logical CPU. A regression is confirmed only when the
median paired candidate/base latency ratio exceeds the historical gate's own
threshold and the bootstrap 95% interval stays above 1.0.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import random
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def command_output(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
    cpu_affinity: set[int] | None = None,
) -> str:
    preexec_fn = None
    if cpu_affinity is not None:
        if not hasattr(os, "sched_setaffinity"):
            raise RuntimeError(
                "CPU affinity requested but os.sched_setaffinity is unavailable"
            )

        def pin_child() -> None:
            os.sched_setaffinity(0, cpu_affinity)

        preexec_fn = pin_child

    proc = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env=env,
        preexec_fn=preexec_fn,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout)
        raise subprocess.CalledProcessError(
            proc.returncode,
            command,
            output=proc.stdout,
        )
    return proc.stdout


def maybe_output(command: list[str], *, cwd: Path = ROOT) -> str | None:
    try:
        return command_output(command, cwd=cwd).strip()
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def measurement_affinity(cpu_mode: str) -> set[int] | None:
    if cpu_mode == "host":
        return None
    if not hasattr(os, "sched_getaffinity") or not hasattr(os, "sched_setaffinity"):
        raise RuntimeError(
            "--cpu-mode single requires sched_getaffinity/sched_setaffinity"
        )
    allowed = sorted(os.sched_getaffinity(0))
    if not allowed:
        raise RuntimeError("empty CPU affinity set")
    return {allowed[0]}


def criterion_filter(benchmarks: list[str]) -> str:
    if not benchmarks:
        raise ValueError("at least one benchmark is required")
    # Regression manifests contain Criterion's filesystem-safe directory names.
    # Criterion itself filters against full IDs: e.g. the directory
    # scheduler_global_owner_dispatch/1000 corresponds to the full ID
    # scheduler/global_owner_dispatch/1000. Either slash or underscore may
    # appear at a sanitized boundary; keep the entire expression anchored.
    escaped = "|".join(re.escape(name).replace("_", "[/_]") for name in benchmarks)
    return rf"^(?:{escaped})$"


def validate_run_count(runs: int) -> None:
    if runs < 2 or runs % 2 != 0:
        raise ValueError("paired confirmation requires an even --runs value >= 2")


def measurement_order(round_idx: int) -> tuple[str, str]:
    return (
        ("base", "candidate")
        if round_idx % 2 == 0
        else ("candidate", "base")
    )


def load_regression_manifest(path: Path) -> list[dict]:
    payload = json.loads(path.read_text())
    if payload.get("schema") != 1:
        raise RuntimeError(f"unsupported regression manifest schema: {payload.get('schema')}")
    rows = payload.get("regressions")
    if not isinstance(rows, list):
        raise RuntimeError("regression manifest is missing a regressions list")

    out = []
    seen = set()
    for row in rows:
        if not isinstance(row, dict):
            raise RuntimeError("regression manifest contains a non-object regression row")
        name = row.get("benchmark")
        threshold = row.get("threshold")
        if not isinstance(name, str) or not name:
            raise RuntimeError("regression manifest contains an invalid benchmark name")
        if name in seen:
            raise RuntimeError(f"duplicate regression benchmark: {name}")
        seen.add(name)
        try:
            threshold = float(threshold)
        except (TypeError, ValueError) as exc:
            raise RuntimeError(f"{name}: invalid regression threshold") from exc
        if not math.isfinite(threshold):
            raise RuntimeError(f"{name}: regression threshold must be finite")
        if threshold < 0:
            raise RuntimeError(f"{name}: regression threshold must be non-negative")
        out.append({**row, "benchmark": name, "threshold": threshold})
    return out


def add_worktree(base_ref: str) -> Path:
    temp_root = Path(tempfile.mkdtemp(prefix="nulang-criterion-base-"))
    base = temp_root / "repo"
    subprocess.run(
        ["git", "worktree", "add", "--detach", str(base), base_ref],
        cwd=ROOT,
        check=True,
    )
    return base


def remove_worktree(base: Path) -> None:
    subprocess.run(
        ["git", "worktree", "remove", "--force", str(base)],
        cwd=ROOT,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    shutil.rmtree(base.parent, ignore_errors=True)


def cargo_environment(target_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    return env


def build_command() -> list[str]:
    return ["cargo", "bench", "--locked", "--bench", "bench_main", "--no-run"]


def bench_command(filter_expr: str) -> list[str]:
    return [
        "cargo",
        "bench",
        "--locked",
        "--bench",
        "bench_main",
        "--",
        filter_expr,
        "--noplot",
        "--color",
        "never",
    ]


def collect_means(criterion_dir: Path) -> dict[str, float]:
    rows: dict[str, float] = {}
    for estimates_path in sorted(criterion_dir.glob("**/new/estimates.json")):
        data = json.loads(estimates_path.read_text())
        point = data.get("mean", {}).get("point_estimate")
        if point is None:
            continue
        rel = estimates_path.parent.parent.relative_to(criterion_dir)
        name = str(rel).replace("\\", "/")
        rows[name] = float(point)
    return rows


def run_filtered_benchmarks(
    *,
    root: Path,
    env: dict[str, str],
    target_dir: Path,
    benchmarks: list[str],
    affinity: set[int] | None,
) -> dict[str, float]:
    criterion_dir = target_dir / "criterion"
    shutil.rmtree(criterion_dir, ignore_errors=True)
    command_output(
        bench_command(criterion_filter(benchmarks)),
        cwd=root,
        env=env,
        cpu_affinity=affinity,
    )
    rows = collect_means(criterion_dir)
    missing = [name for name in benchmarks if name not in rows]
    if missing:
        raise RuntimeError(
            "Criterion confirmation did not emit estimates for: " + ", ".join(missing)
        )
    return {name: rows[name] for name in benchmarks}


def _bootstrap_median_ci(
    values: list[float],
    *,
    iterations: int = 4_000,
) -> tuple[float, float]:
    if not values:
        raise ValueError("bootstrap requires at least one value")
    if len(values) == 1:
        return values[0], values[0]

    rng = random.Random(0x43524954)
    n = len(values)
    medians = [
        statistics.median(values[rng.randrange(n)] for _ in range(n))
        for _ in range(iterations)
    ]
    medians.sort()
    lower_idx = int(0.025 * (iterations - 1))
    upper_idx = int(0.975 * (iterations - 1))
    return medians[lower_idx], medians[upper_idx]


def analyze_pairs(
    *,
    benchmark: str,
    threshold: float,
    base_ns: list[float],
    candidate_ns: list[float],
) -> dict[str, float | int | str | bool]:
    if not base_ns or len(base_ns) != len(candidate_ns):
        raise RuntimeError(
            f"{benchmark}: paired sample counts differ "
            f"(base={len(base_ns)}, candidate={len(candidate_ns)})"
        )
    ratios = []
    for index, (base, candidate) in enumerate(zip(base_ns, candidate_ns)):
        if base <= 0 or candidate <= 0:
            raise RuntimeError(f"{benchmark}: non-positive sample in pair {index}")
        ratios.append(candidate / base)

    lower, upper = _bootstrap_median_ci(ratios)
    median_ratio = statistics.median(ratios)
    confirmed = median_ratio > 1.0 + threshold and lower > 1.0
    return {
        "benchmark": benchmark,
        "pairs": len(ratios),
        "threshold": threshold,
        "median_latency_ratio": median_ratio,
        "median_latency_change_pct": (median_ratio - 1.0) * 100.0,
        "latency_ratio_ci95_lower": lower,
        "latency_ratio_ci95_upper": upper,
        "confirmed": confirmed,
    }


def cpu_model() -> str | None:
    path = Path("/proc/cpuinfo")
    if not path.exists():
        return None
    for line in path.read_text(errors="replace").splitlines():
        if line.lower().startswith("model name") and ":" in line:
            return line.split(":", 1)[1].strip()
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--runs", type=int, default=8)
    parser.add_argument(
        "--cpu-mode",
        choices=("single", "host"),
        default="single",
        help="pin measured runs to one logical CPU by default",
    )
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    try:
        validate_run_count(args.runs)
    except ValueError as exc:
        parser.error(str(exc))

    regressions = load_regression_manifest(args.manifest)
    if not regressions:
        print("No historical regressions require paired confirmation.")
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "status": "not_required",
                        "methodology": "same-host same-CPU round-paired Criterion parent-vs-candidate confirmation",
                        "base_ref": args.base_ref,
                        "base_sha": maybe_output(["git", "rev-parse", args.base_ref]),
                        "candidate_sha": maybe_output(["git", "rev-parse", "HEAD"]),
                        "measured_runs": 0,
                        "environment": None,
                        "historical_regressions": [],
                        "samples": {"base": {}, "candidate": {}},
                        "analysis": [],
                        "confirmed": [],
                    },
                    indent=2,
                    sort_keys=True,
                )
                + "\n"
            )
        return 0

    benchmarks = [row["benchmark"] for row in regressions]
    thresholds = {row["benchmark"]: float(row["threshold"]) for row in regressions}
    affinity = measurement_affinity(args.cpu_mode)
    base = add_worktree(args.base_ref)
    target_root = Path(tempfile.mkdtemp(prefix="nulang-criterion-target-"))

    try:
        roots = {"base": base, "candidate": ROOT}
        targets = {
            "base": target_root / "base",
            "candidate": target_root / "candidate",
        }
        envs = {
            variant: cargo_environment(targets[variant])
            for variant in ("base", "candidate")
        }

        for variant in ("base", "candidate"):
            print(f"[build] {variant}", flush=True)
            command_output(
                build_command(),
                cwd=roots[variant],
                env=envs[variant],
            )

        samples: dict[str, dict[str, list[float]]] = {
            "base": {name: [] for name in benchmarks},
            "candidate": {name: [] for name in benchmarks},
        }

        for round_idx in range(args.runs):
            for variant in measurement_order(round_idx):
                print(f"[pair {round_idx + 1}] {variant}", flush=True)
                measured = run_filtered_benchmarks(
                    root=roots[variant],
                    env=envs[variant],
                    target_dir=targets[variant],
                    benchmarks=benchmarks,
                    affinity=affinity,
                )
                for name, mean_ns in measured.items():
                    samples[variant][name].append(mean_ns)

        analysis = [
            analyze_pairs(
                benchmark=name,
                threshold=thresholds[name],
                base_ns=samples["base"][name],
                candidate_ns=samples["candidate"][name],
            )
            for name in benchmarks
        ]
        confirmed = [row for row in analysis if bool(row["confirmed"])]

        print()
        print("paired Criterion confirmation")
        print(
            "benchmark                                  pairs   latency delta   threshold   95% ratio CI   result"
        )
        print(
            "-----------------------------------------  -----   -------------   ---------   -------------   ---------"
        )
        for row in analysis:
            result = "REGRESSION" if row["confirmed"] else "not confirmed"
            print(
                f"{str(row['benchmark']):<41}  {int(row['pairs']):>5}   "
                f"{float(row['median_latency_change_pct']):>+12.1f}%   "
                f"{float(row['threshold']) * 100:>8.1f}%   "
                f"[{float(row['latency_ratio_ci95_lower']):.3f}, "
                f"{float(row['latency_ratio_ci95_upper']):.3f}]   {result}"
            )

        report = {
            "schema": 1,
            "status": "confirmed" if confirmed else "clear",
            "methodology": "same-host same-CPU round-paired Criterion parent-vs-candidate confirmation",
            "base_ref": args.base_ref,
            "base_sha": maybe_output(["git", "rev-parse", "HEAD"], cwd=base),
            "candidate_sha": maybe_output(["git", "rev-parse", "HEAD"], cwd=ROOT),
            "measured_runs": args.runs,
            "environment": {
                "generated_at": datetime.now(timezone.utc).isoformat(),
                "os": platform.platform(),
                "machine": platform.machine(),
                "cpu_count": os.cpu_count(),
                "cpu_model": cpu_model(),
                "measurement_cpu_mode": args.cpu_mode,
                "measurement_cpu_affinity": (
                    sorted(affinity) if affinity is not None else None
                ),
                "rustc": maybe_output(["rustc", "--version"]),
                "cargo": maybe_output(["cargo", "--version"]),
            },
            "historical_regressions": regressions,
            "samples": samples,
            "analysis": analysis,
            "confirmed": [row["benchmark"] for row in confirmed],
        }
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
            print(f"wrote {args.output}")

        if confirmed:
            print(
                f"CONFIRMED: {len(confirmed)} benchmark regression(s) reproduced "
                "against the exact base on the same host."
            )
            return 1

        print(
            "OK: historical alerts did not reproduce as candidate regressions "
            "against the exact base on the same host."
        )
        return 0
    finally:
        remove_worktree(base)
        shutil.rmtree(target_root, ignore_errors=True)


def entrypoint() -> int:
    try:
        return main()
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError) as exc:
        print(f"ERROR: paired benchmark confirmation failed: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(entrypoint())
