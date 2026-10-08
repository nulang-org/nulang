#!/usr/bin/env python3
"""Opt-in Linux perf sampling of the actual non-durable Nulang Savina runner.

This is diagnostic instrumentation, NOT a timed A/B benchmark: sampling,
debug symbols, and frame pointers change runtime cost. Use
scripts/nulang_ab_bench.py for throughput/regression confirmation.
"""
from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKLOADS = ("counting", "ping_pong", "thread_ring", "fork_join", "skynet")


def validate_options(workload: str, repeat: int, frequency: int) -> None:
    if workload not in WORKLOADS:
        raise ValueError(f"unknown workload: {workload}")
    if repeat < 1:
        raise ValueError("repeat must be positive")
    if not 1 <= frequency <= 999:
        raise ValueError("frequency must be between 1 and 999 Hz")


def benchmark_command(binary: Path, workload: str, repeat: int) -> list[str]:
    validate_options(workload, repeat, 99)
    return [
        str(binary), "--benchmark", workload,
        "--repeat", str(repeat), "--format", "jsonl",
    ]


def perf_command(
    perf: Path, output: Path, binary: Path,
    workload: str, repeat: int, frequency: int,
) -> list[str]:
    validate_options(workload, repeat, frequency)
    return [
        str(perf), "record", "--call-graph", "fp",
        "--freq", str(frequency), "--output", str(output), "--",
        *benchmark_command(binary, workload, repeat),
    ]


def parse_samples(output: str, workload: str, repeat: int) -> list[dict[str, object]]:
    validate_options(workload, repeat, 99)
    results: list[dict[str, object]] = []
    for line in output.splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as exc:
            raise RuntimeError("non-JSON output from nulang-savina") from exc
        if not isinstance(row, dict):
            raise RuntimeError("expected one JSON object per sample")
        if row.get("runtime") != "nulang":
            raise RuntimeError("unexpected benchmark runtime")
        if row.get("benchmark") != workload:
            raise RuntimeError("wrong benchmark in profile output")
        if row.get("suite") != "savina-style" or row.get("schema") != 1:
            raise RuntimeError("unsupported benchmark record schema")
        try:
            iteration = row["iteration"]
            messages = row["messages"]
            elapsed = row["elapsed_ns"]
            if any(type(x) is not int or x <= 0 for x in (iteration, messages, elapsed)):
                raise ValueError("counts must be positive integers")
        except (KeyError, ValueError) as exc:
            raise RuntimeError("iteration/messages/elapsed must be positive integers") from exc
        results.append(row)

    if len(results) != repeat:
        raise RuntimeError(f"sample count {len(results)} != requested repeat {repeat}")
    if sorted(row["iteration"] for row in results) != list(range(1, repeat + 1)):
        raise RuntimeError("missing, invalid, or duplicate iteration numbers")
    return results


def version(command: list[str]) -> str | None:
    try:
        result = subprocess.run(
            command, cwd=ROOT, capture_output=True,
            text=True, check=False, timeout=30,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        pass
    return None


def measure_cpu_set(mode: str) -> set[int] | None:
    if mode == "host":
        return None
    if not hasattr(os, "sched_getaffinity") or not hasattr(os, "sched_setaffinity"):
        raise RuntimeError("single-core profiling requires Linux CPU affinity")
    allowed = os.sched_getaffinity(0)
    if not allowed:
        raise RuntimeError("current process has an empty allowed CPU set")
    return {min(allowed)}


def run_profile(args: argparse.Namespace) -> int:
    validate_options(args.workload, args.repeat, args.frequency)
    if platform.system() != "Linux":
        raise RuntimeError("perf profiling requires Linux")
    perf = shutil.which("perf")
    cargo = shutil.which("cargo")
    if not perf or not cargo:
        raise RuntimeError("Linux perf and Rust cargo must both be installed")

    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)

    # Exact compiler/runtime artifact, isolated from JIT/AOT and optional services.
    # Unlike performance baselines, this instrumented build keeps symbols and
    # frame pointers for perf; timing is only a sanity signal.
    build_env = os.environ.copy()
    build_env["CARGO_PROFILE_SAVINA_DEBUG"] = "1"
    flags = build_env.get("RUSTFLAGS", "").strip()
    build_env["RUSTFLAGS"] = (flags + " -Cforce-frame-pointers=yes").strip()

    build = subprocess.run([
        cargo, "build", "--locked", "--profile", "savina",
        "--no-default-features", "--features", "savina-bench",
        "--bin", "nulang-savina",
    ], cwd=ROOT, env=build_env, capture_output=True, text=True)
    if build.returncode != 0:
        (output / "build.stderr.txt").write_text(build.stderr)
        raise RuntimeError("Nulang profiling build failed; see build.stderr.txt")

    meta = subprocess.run([
        cargo, "metadata", "--no-deps", "--format-version", "1",
    ], cwd=ROOT, capture_output=True, text=True, check=True)
    target_dir = Path(json.loads(meta.stdout)["target_directory"])
    binary = target_dir / "savina" / "nulang-savina"
    if not binary.is_file():
        raise RuntimeError(f"Nulang profiler binary missing: {binary}")

    affinity = measure_cpu_set(args.cpu_mode)
    def pin_child() -> None:
        if affinity is not None:
            os.sched_setaffinity(0, affinity)

    capture_path = output / "perf.data"
    command = perf_command(
        Path(perf), capture_path, binary,
        args.workload, args.repeat, args.frequency,
    )
    result = subprocess.run(
        command, cwd=ROOT, capture_output=True, text=True,
        preexec_fn=pin_child if affinity is not None else None,
    )
    (output / "samples.jsonl").write_text(result.stdout)
    (output / "perf.stderr.txt").write_text(result.stderr)
    if result.returncode != 0:
        raise RuntimeError(
            "perf record failed (check perf_event_paranoid/permissions); "
            "see perf.stderr.txt"
        )

    samples = parse_samples(result.stdout, args.workload, args.repeat)
    report_cmd = [
        perf, "report", "--stdio", "--input", str(capture_path),
        "--sort", "comm,dso,symbol", "--percent-limit", "0.5",
    ]
    report = subprocess.run(
        report_cmd, cwd=ROOT, capture_output=True, text=True,
    )
    (output / "perf-report.txt").write_text(report.stdout)
    (output / "perf-report.stderr.txt").write_text(report.stderr)
    if report.returncode != 0:
        raise RuntimeError("perf report failed; see perf-report.stderr.txt")

    metadata = {
        "schema": 1,
        "methodology": "Linux perf sampling of instrumented non-durable actor workload; not A/B",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "git_sha": version(["git", "rev-parse", "HEAD"]),
        "rustc": version(["rustc", "--version"]),
        "perf": version([perf, "--version"]),
        "os": platform.platform(),
        "cpu_mode": args.cpu_mode,
        "cpu_affinity": sorted(affinity) if affinity is not None else None,
        "frequency_hz": args.frequency,
        "profile_build_env": {
            "CARGO_PROFILE_SAVINA_DEBUG": build_env["CARGO_PROFILE_SAVINA_DEBUG"],
            "RUSTFLAGS": build_env["RUSTFLAGS"],
        },
        "workload": args.workload,
        "samples": samples,
    }
    (output / "manifest.json").write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n")
    print(f"Profile captured: {output}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workload", default="ping_pong", choices=WORKLOADS)
    parser.add_argument("--repeat", type=int, default=25)
    parser.add_argument("--frequency", type=int, default=99)
    parser.add_argument("--cpu-mode", choices=("single", "host"), default="single")
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        return run_profile(args)
    except (RuntimeError, ValueError) as exc:
        print(f"profiling failed: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
