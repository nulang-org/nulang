#!/usr/bin/env python3
"""Exact-base A/B harness for Nulang actor construction and idle density.

The existing ``examples/actor_density.rs`` probe is deliberately process-oriented:
each mode runs in a fresh process so allocator retention from another mode cannot
contaminate RSS measurements. This harness preserves that boundary, builds the
same example for an exact base worktree and the candidate checkout, then runs
paired measurements on one host with counterbalanced variant/mode ordering.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import random
import shutil
import statistics
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TARGET_ROOT = Path(tempfile.gettempdir()) / "nulang-density-ab-target"
MODES = ("construct", "idle")


def _parse_key_values(output: str) -> dict[str, str]:
    fields: dict[str, str] = {}
    for raw in output.splitlines():
        if "=" not in raw:
            continue
        key, value = raw.split("=", 1)
        key = key.strip()
        value = value.strip()
        if key:
            fields[key] = value
    return fields


def _required(fields: dict[str, str], key: str) -> str:
    try:
        return fields[key]
    except KeyError as exc:
        raise RuntimeError(f"actor-density output missing {key}") from exc


def _optional_float(fields: dict[str, str], key: str) -> float | None:
    value = _required(fields, key)
    if value == "unavailable":
        return None
    try:
        return float(value)
    except ValueError as exc:
        raise RuntimeError(f"invalid {key}: {value}") from exc


def parse_probe(output: str, expected_mode: str) -> dict[str, float | int | None | str]:
    if expected_mode not in MODES:
        raise ValueError(f"unsupported actor-density mode: {expected_mode}")

    fields = _parse_key_values(output)
    mode = _required(fields, "mode")
    if mode != expected_mode:
        raise RuntimeError(f"expected mode {expected_mode}, got {mode}")

    try:
        actor_count = int(_required(fields, "actor_count"))
        actor_struct_bytes = int(_required(fields, "actor_struct_bytes"))
    except ValueError as exc:
        raise RuntimeError("actor-density integer field is malformed") from exc
    if actor_count <= 0 or actor_struct_bytes <= 0:
        raise RuntimeError("actor-density counts and layout sizes must be positive")

    if mode == "construct":
        ns_key = "construct_ns_per_item"
        rss_key = "rss_construct_delta_bytes_per_actor"
        settle_seconds = None
        heap_used = None
    else:
        ns_key = "spawn_ns_per_item"
        rss_key = "rss_spawn_delta_bytes_per_actor"
        settle_seconds = _optional_float(fields, "scheduler_settle_seconds")
        heap_used = _optional_float(fields, "actor_heap_used_bytes_per_actor")

    try:
        ns_per_actor = float(_required(fields, ns_key))
    except ValueError as exc:
        raise RuntimeError(f"invalid {ns_key}") from exc
    if ns_per_actor <= 0:
        raise RuntimeError(f"{ns_key} must be positive")

    rss_bytes_per_actor = _optional_float(fields, rss_key)
    if rss_bytes_per_actor is not None and rss_bytes_per_actor < 0:
        raise RuntimeError(f"{rss_key} must be non-negative")

    return {
        "mode": mode,
        "actor_count": actor_count,
        "actor_struct_bytes": actor_struct_bytes,
        "ns_per_actor": ns_per_actor,
        "rss_bytes_per_actor": rss_bytes_per_actor,
        "scheduler_settle_seconds": settle_seconds,
        "actor_heap_used_bytes_per_actor": heap_used,
    }


def _bootstrap_median_ci(values: list[float], iterations: int = 4_000) -> tuple[float, float]:
    if not values:
        raise ValueError("bootstrap requires at least one value")
    if len(values) == 1:
        return values[0], values[0]
    rng = random.Random(0x44454E53)
    n = len(values)
    medians = [
        statistics.median(values[rng.randrange(n)] for _ in range(n))
        for _ in range(iterations)
    ]
    medians.sort()
    return (
        medians[int(0.025 * (iterations - 1))],
        medians[int(0.975 * (iterations - 1))],
    )


def paired_lower_is_better(base: list[float], candidate: list[float]) -> dict[str, float | int]:
    if len(base) != len(candidate):
        raise RuntimeError(
            f"paired sample count changed (base={len(base)}, candidate={len(candidate)})"
        )
    if not base:
        raise RuntimeError("paired comparison requires at least one sample")

    improvements: list[float] = []
    reductions: list[float] = []
    for index, (base_value, candidate_value) in enumerate(zip(base, candidate)):
        if base_value <= 0 or candidate_value <= 0:
            raise RuntimeError(f"pair {index}: values must be positive")
        improvement = base_value / candidate_value
        improvements.append(improvement)
        reductions.append((1.0 - candidate_value / base_value) * 100.0)

    lower, upper = _bootstrap_median_ci(improvements)
    return {
        "pairs": len(improvements),
        "median_improvement_x": statistics.median(improvements),
        "median_reduction_pct": statistics.median(reductions),
        "ci95_lower": lower,
        "ci95_upper": upper,
    }


def measurement_order(round_index: int) -> list[tuple[str, str]]:
    if round_index < 0:
        raise ValueError("round index must be non-negative")
    variants = ["base", "candidate"]
    modes = list(MODES)
    if round_index % 2 == 1:
        variants.reverse()
        modes.reverse()
    return [(variant, mode) for variant in variants for mode in modes]


def cargo_environment(variant: str) -> dict[str, str]:
    if variant not in {"base", "candidate"}:
        raise ValueError(f"unknown A/B variant: {variant}")
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(TARGET_ROOT / variant)
    return env


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
            raise RuntimeError("CPU affinity requested but unavailable")

        def pin_child() -> None:
            os.sched_setaffinity(0, cpu_affinity)

        preexec_fn = pin_child

    proc = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=preexec_fn,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout)
        raise subprocess.CalledProcessError(proc.returncode, command, output=proc.stdout)
    return proc.stdout


def measurement_affinity(cpu_mode: str) -> set[int] | None:
    if cpu_mode == "host":
        return None
    if not hasattr(os, "sched_getaffinity") or not hasattr(os, "sched_setaffinity"):
        raise RuntimeError("--cpu-mode single requires Linux CPU affinity support")
    allowed = sorted(os.sched_getaffinity(0))
    if not allowed:
        raise RuntimeError("empty CPU affinity set")
    return {allowed[0]}


def cargo_feature_args(no_default_features: bool, features: str) -> list[str]:
    out: list[str] = []
    if no_default_features:
        out.append("--no-default-features")
    if features:
        out.extend(["--features", features])
    return out


def build_command(extra_args: list[str]) -> list[str]:
    return ["cargo", "build", "--release", *extra_args, "--example", "actor_density"]


def binary_path(env: dict[str, str]) -> Path:
    suffix = ".exe" if os.name == "nt" else ""
    return Path(env["CARGO_TARGET_DIR"]) / "release" / "examples" / f"actor_density{suffix}"


def add_worktree(base_ref: str) -> Path:
    temp_root = Path(tempfile.mkdtemp(prefix="nulang-density-ab-base-"))
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


def git_output(args: list[str], cwd: Path) -> str | None:
    try:
        return command_output(["git", *args], cwd=cwd).strip()
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def summarize_mode(rows: list[dict[str, float | int | None | str]]) -> dict[str, float | int | None]:
    ns = [float(row["ns_per_actor"]) for row in rows]
    rss = [
        float(row["rss_bytes_per_actor"])
        for row in rows
        if row["rss_bytes_per_actor"] is not None
    ]
    layouts = {int(row["actor_struct_bytes"]) for row in rows}
    counts = {int(row["actor_count"]) for row in rows}
    if len(layouts) != 1 or len(counts) != 1:
        raise RuntimeError("actor-density layout/count changed across samples")
    return {
        "samples": len(rows),
        "actor_count": counts.pop(),
        "actor_struct_bytes": layouts.pop(),
        "median_ns_per_actor": statistics.median(ns),
        "median_rss_bytes_per_actor": statistics.median(rss) if rss else None,
    }


def comparisons(samples: dict[str, dict[str, list[dict[str, float | int | None | str]]]]) -> dict[str, object]:
    out: dict[str, object] = {}
    for mode in MODES:
        base_rows = samples["base"][mode]
        candidate_rows = samples["candidate"][mode]
        time = paired_lower_is_better(
            [float(row["ns_per_actor"]) for row in base_rows],
            [float(row["ns_per_actor"]) for row in candidate_rows],
        )
        base_rss = [row["rss_bytes_per_actor"] for row in base_rows]
        candidate_rss = [row["rss_bytes_per_actor"] for row in candidate_rows]
        rss = None
        if all(value is not None for value in base_rss + candidate_rss):
            rss = paired_lower_is_better(
                [float(value) for value in base_rss if value is not None],
                [float(value) for value in candidate_rss if value is not None],
            )
        out[mode] = {"time": time, "rss": rss}

    base_layout = int(samples["base"]["idle"][0]["actor_struct_bytes"])
    candidate_layout = int(samples["candidate"]["idle"][0]["actor_struct_bytes"])
    out["actor_layout"] = {
        "base_bytes": base_layout,
        "candidate_bytes": candidate_layout,
        "reduction_bytes": base_layout - candidate_layout,
        "reduction_pct": (1.0 - candidate_layout / base_layout) * 100.0,
    }
    return out


def print_report(summary: dict[str, dict[str, dict[str, float | int | None]]], compare: dict[str, object]) -> None:
    print()
    print("actor-density exact-base A/B")
    print("mode       base ns/actor   candidate ns/actor   time reduction   RSS reduction")
    print("---------  -------------   ------------------   --------------   -------------")
    for mode in MODES:
        base = summary["base"][mode]
        candidate = summary["candidate"][mode]
        result = compare[mode]
        assert isinstance(result, dict)
        time = result["time"]
        rss = result["rss"]
        assert isinstance(time, dict)
        rss_text = "unavailable"
        if isinstance(rss, dict):
            rss_text = f"{float(rss['median_reduction_pct']):+.2f}%"
        print(
            f"{mode:<9}  {float(base['median_ns_per_actor']):>13.1f}   "
            f"{float(candidate['median_ns_per_actor']):>18.1f}   "
            f"{float(time['median_reduction_pct']):>+13.2f}%   {rss_text:>13}"
        )
    layout = compare["actor_layout"]
    assert isinstance(layout, dict)
    print(
        f"Actor layout: {layout['base_bytes']} -> {layout['candidate_bytes']} bytes "
        f"({float(layout['reduction_pct']):+.2f}% reduction)"
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--actors", type=int, default=10_000)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--cpu-mode", choices=("single", "host"), default="single")
    parser.add_argument("--no-default-features", action="store_true")
    parser.add_argument("--features", default="")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    if args.actors < 1 or args.runs < 1 or args.warmup < 0:
        parser.error("--actors/--runs must be >= 1 and --warmup must be >= 0")

    affinity = measurement_affinity(args.cpu_mode)
    cargo_args = cargo_feature_args(args.no_default_features, args.features)
    base = add_worktree(args.base_ref)
    roots = {"base": base, "candidate": ROOT}
    environments = {
        "base": cargo_environment("base"),
        "candidate": cargo_environment("candidate"),
    }
    try:
        for variant in ("base", "candidate"):
            probe = roots[variant] / "examples" / "actor_density.rs"
            if not probe.exists():
                raise RuntimeError(f"{variant} ref does not contain examples/actor_density.rs")
            print(f"[build] {variant}", flush=True)
            command_output(
                build_command(cargo_args),
                cwd=roots[variant],
                env=environments[variant],
            )

        samples: dict[str, dict[str, list[dict[str, float | int | None | str]]]] = {
            variant: {mode: [] for mode in MODES}
            for variant in ("base", "candidate")
        }
        total_rounds = args.warmup + args.runs
        for round_index in range(total_rounds):
            measured = round_index >= args.warmup
            label = "sample" if measured else "warmup"
            display_index = round_index - args.warmup + 1 if measured else round_index + 1
            for variant, mode in measurement_order(round_index):
                print(f"[{label} {display_index}] {variant}/{mode}", flush=True)
                output = command_output(
                    [str(binary_path(environments[variant])), mode, str(args.actors)],
                    cwd=roots[variant],
                    env=environments[variant],
                    cpu_affinity=affinity,
                )
                row = parse_probe(output, mode)
                if measured:
                    samples[variant][mode].append(row)

        summary = {
            variant: {
                mode: summarize_mode(samples[variant][mode])
                for mode in MODES
            }
            for variant in ("base", "candidate")
        }
        compare = comparisons(samples)
        print_report(summary, compare)

        report = {
            "schema": 1,
            "methodology": "same-host paired exact-base actor construction and idle-density A/B",
            "base_ref": args.base_ref,
            "base_sha": git_output(["rev-parse", "HEAD"], base),
            "candidate_sha": git_output(["rev-parse", "HEAD"], ROOT),
            "actors": args.actors,
            "warmup_runs": args.warmup,
            "measured_runs": args.runs,
            "cargo_args": cargo_args,
            "environment": {
                "generated_at": datetime.now(timezone.utc).isoformat(),
                "os": platform.platform(),
                "machine": platform.machine(),
                "cpu_count": os.cpu_count(),
                "measurement_cpu_mode": args.cpu_mode,
                "measurement_cpu_affinity": sorted(affinity) if affinity is not None else None,
            },
            "samples": samples,
            "summary": summary,
            "comparison": compare,
        }
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
            print(f"wrote {args.output}")
        return 0
    finally:
        remove_worktree(base)


if __name__ == "__main__":
    raise SystemExit(main())
