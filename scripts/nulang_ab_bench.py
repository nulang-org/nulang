#!/usr/bin/env python3
"""Compare Nulang actor benchmarks at the current checkout vs an exact base ref.

Both variants are built once, then benchmark rounds alternate execution order on
one host. Measured child processes default to the same single logical CPU so
scheduler topology and host load are less likely to masquerade as a runtime
optimization.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BENCHMARKS = ("counting", "ping_pong", "thread_ring", "fork_join")
RECORD_RE = re.compile(
    r"\[cross-bench\]\s+runtime=nulang\s+"
    r"benchmark=(?P<benchmark>[a-z0-9_-]+)\s+"
    r"messages=(?P<messages>\d+)\s+elapsed_ns=(?P<elapsed_ns>\d+)"
)
AB_RECORD_RE = re.compile(
    r"\[ab-bench\]\s+benchmark=(?P<benchmark>[a-z0-9_-]+)\s+"
    r"operations=(?P<messages>\d+)\s+elapsed_ns=(?P<elapsed_ns>\d+)"
)


def command_output(
    command: list[str],
    *,
    cwd: Path,
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


def parse_records(output: str) -> dict[str, dict[str, int]]:
    rows: dict[str, dict[str, int]] = {}
    for match in RECORD_RE.finditer(output):
        rows[match.group("benchmark")] = {
            "messages": int(match.group("messages")),
            "elapsed_ns": int(match.group("elapsed_ns")),
        }
    for match in AB_RECORD_RE.finditer(output):
        rows["ab/" + match.group("benchmark")] = {
            "messages": int(match.group("messages")),
            "elapsed_ns": int(match.group("elapsed_ns")),
        }
    missing = [name for name in BENCHMARKS if name not in rows]
    if missing:
        raise RuntimeError(
            "Nulang benchmark output missing records for "
            + ", ".join(missing)
            + "\n"
            + output
        )
    return rows

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


def cargo_command(extra_args: list[str]) -> list[str]:
    return [
        "cargo",
        "test",
        "--release",
        *extra_args,
        "benchmarks::bench_",
        "--",
        "--nocapture",
        "--test-threads=1",
    ]


def cargo_build_command(extra_args: list[str]) -> list[str]:
    return [
        "cargo",
        "test",
        "--release",
        "--no-run",
        *extra_args,
        "benchmarks::bench_",
    ]


def add_worktree(base_ref: str) -> Path:
    temp_root = Path(tempfile.mkdtemp(prefix="nulang-ab-base-"))
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


def summarize(
    samples: dict[str, dict[str, list[dict[str, int]]]]
) -> dict[str, dict[str, dict[str, float | int]]]:
    summary: dict[str, dict[str, dict[str, float | int]]] = {}
    for variant, workloads in samples.items():
        summary[variant] = {}
        for name, rows in workloads.items():
            if not rows:
                continue
            elapsed = [row["elapsed_ns"] for row in rows]
            counts = {row["messages"] for row in rows}
            if len(counts) != 1:
                raise RuntimeError(f"{variant}/{name}: message count changed")
            messages = counts.pop()
            median_ns = statistics.median(elapsed)
            summary[variant][name] = {
                "samples": len(rows),
                "messages": messages,
                "median_elapsed_ns": int(median_ns),
                "min_elapsed_ns": min(elapsed),
                "max_elapsed_ns": max(elapsed),
                "median_ns_per_message": median_ns / messages,
                "median_messages_per_second": messages * 1_000_000_000 / median_ns,
            }
    return summary


def comparisons(
    summary: dict[str, dict[str, dict[str, float | int]]]
) -> dict[str, dict[str, float]]:
    out: dict[str, dict[str, float]] = {}
    common = set(summary["base"]) & set(summary["candidate"])
    for name in sorted(common):
        base_ns = float(summary["base"][name]["median_elapsed_ns"])
        candidate_ns = float(summary["candidate"][name]["median_elapsed_ns"])
        base_mps = float(summary["base"][name]["median_messages_per_second"])
        candidate_mps = float(summary["candidate"][name]["median_messages_per_second"])
        out[name] = {
            "candidate_speedup_x": base_ns / candidate_ns,
            "candidate_throughput_change_pct": (candidate_mps / base_mps - 1.0) * 100.0,
            "candidate_latency_change_pct": (candidate_ns / base_ns - 1.0) * 100.0,
        }
    return out


def print_table(
    summary: dict[str, dict[str, dict[str, float | int]]],
    compare: dict[str, dict[str, float]],
) -> None:
    print()
    print(
        "benchmark            base ops/s   candidate ops/s   throughput delta   speedup"
    )
    print(
        "-------------------  -----------  -----------------  -----------------  -------"
    )
    ordered = [name for name in BENCHMARKS if name in compare]
    ordered.extend(sorted(name for name in compare if name not in BENCHMARKS))
    for name in ordered:
        base = float(summary["base"][name]["median_messages_per_second"])
        candidate = float(summary["candidate"][name]["median_messages_per_second"])
        delta = compare[name]["candidate_throughput_change_pct"]
        speedup = compare[name]["candidate_speedup_x"]
        print(
            f"{name:<19}  {base:>11,.0f}  {candidate:>17,.0f}  "
            f"{delta:>+16.2f}%  {speedup:>6.3f}x"
        )

def print_candidate_only(
    summary: dict[str, dict[str, dict[str, float | int]]]
) -> None:
    base_names = set(summary["base"])
    candidate_only = sorted(set(summary["candidate"]) - base_names)
    if not candidate_only:
        return

    print()
    print("candidate-only diagnostics")
    print("--------------------------")
    for name in candidate_only:
        row = summary["candidate"][name]
        ops = float(row["median_messages_per_second"])
        ns_per_op = float(row["median_ns_per_message"])
        print(f"{name:<32} {ops:>14,.0f} ops/s  {ns_per_op:>10.1f} ns/op")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument(
        "--cpu-mode", choices=("single", "host"), default="single"
    )
    parser.add_argument(
        "--no-default-features",
        action="store_true",
        help="pass --no-default-features to both variants",
    )
    parser.add_argument(
        "--features",
        default="",
        help="comma-separated Cargo features enabled for both variants",
    )
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    if args.runs < 1 or args.warmup < 0:
        parser.error("--runs must be >= 1 and --warmup must be >= 0")

    affinity = measurement_affinity(args.cpu_mode)
    extra_args: list[str] = []
    if args.no_default_features:
        extra_args.append("--no-default-features")
    if args.features:
        extra_args.extend(["--features", args.features])

    base = add_worktree(args.base_ref)
    try:
        # The A/B parser depends on the machine-readable record added by the
        # benchmark foundation. Fail early if either side predates it.
        for label, root in (("base", base), ("candidate", ROOT)):
            source = (root / "src" / "benchmarks.rs").read_text(errors="replace")
            if "[cross-bench]" not in source:
                raise RuntimeError(
                    f"{label} ref does not contain the cross-runtime benchmark "
                    "record format; merge/rebase the benchmark foundation first"
                )

        print(f"[build] base={args.base_ref}", flush=True)
        command_output(cargo_build_command(extra_args), cwd=base)
        print("[build] candidate=HEAD", flush=True)
        command_output(cargo_build_command(extra_args), cwd=ROOT)

        samples: dict[str, dict[str, list[dict[str, int]]]] = {
            variant: {name: [] for name in BENCHMARKS}
            for variant in ("base", "candidate")
        }
        roots = {"base": base, "candidate": ROOT}
        rounds = args.warmup + args.runs
        for round_idx in range(rounds):
            measured = round_idx >= args.warmup
            phase = "sample" if measured else "warmup"
            index = round_idx - args.warmup + 1 if measured else round_idx + 1

            # Alternate ordering so a monotonic temperature/load drift does not
            # systematically favor the same variant.
            order = (
                ("base", "candidate")
                if round_idx % 2 == 0
                else ("candidate", "base")
            )
            for variant in order:
                print(f"[{phase} {index}] {variant}", flush=True)
                output = command_output(
                    cargo_command(extra_args),
                    cwd=roots[variant],
                    cpu_affinity=affinity,
                )
                records = parse_records(output)
                if measured:
                    for name, row in records.items():
                        samples[variant].setdefault(name, []).append(row)

        summary = summarize(samples)
        compare = comparisons(summary)
        print_table(summary, compare)
        print_candidate_only(summary)

        allowed = (
            sorted(os.sched_getaffinity(0))
            if hasattr(os, "sched_getaffinity")
            else None
        )
        report = {
            "schema": 1,
            "methodology": "same-host same-CPU Nulang base-vs-candidate actor A/B",
            "base_ref": args.base_ref,
            "base_sha": maybe_output(["git", "rev-parse", "HEAD"], cwd=base),
            "candidate_sha": maybe_output(["git", "rev-parse", "HEAD"]),
            "warmup_runs": args.warmup,
            "measured_runs": args.runs,
            "cargo_args": extra_args,
            "environment": {
                "generated_at": datetime.now(timezone.utc).isoformat(),
                "os": platform.platform(),
                "machine": platform.machine(),
                "cpu_count": os.cpu_count(),
                "cpu_model": next(
                    (
                        line.split(":", 1)[1].strip()
                        for line in Path("/proc/cpuinfo").read_text(
                            errors="replace"
                        ).splitlines()
                        if line.lower().startswith("model name") and ":" in line
                    ),
                    None,
                )
                if Path("/proc/cpuinfo").exists()
                else None,
                "host_allowed_cpus": allowed,
                "measurement_cpu_mode": args.cpu_mode,
                "measurement_cpu_affinity": (
                    sorted(affinity) if affinity is not None else None
                ),
                "rustc": maybe_output(["rustc", "--version"]),
                "cargo": maybe_output(["cargo", "--version"]),
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
