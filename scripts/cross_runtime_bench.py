#!/usr/bin/env python3
"""Run matched Savina-style messaging baselines on one host.

The default set compares standard runtime primitives. Optional Pony and CAF
fixtures add actor-runtime baselines without changing the default dependency
set. See benchmarks/cross_runtime/README.md before interpreting or publishing
results.
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
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "benchmarks" / "cross_runtime"
BUILD = ROOT / "target" / "cross-runtime"
BENCHMARKS = ("counting", "ping_pong", "thread_ring", "fork_join")
RECORD_RE = re.compile(
    r"\[cross-bench\]\s+runtime=(?P<runtime>[a-z0-9_-]+)\s+"
    r"benchmark=(?P<benchmark>[a-z0-9_-]+)\s+"
    r"messages=(?P<messages>\d+)\s+elapsed_ns=(?P<elapsed_ns>\d+)"
)


def command_output(
    command: list[str],
    *,
    cwd: Path = ROOT,
    cpu_affinity: set[int] | None = None,
) -> str:
    """Run one command, optionally constraining the child to specific CPUs."""

    preexec_fn = None
    if cpu_affinity is not None:
        if not hasattr(os, "sched_setaffinity"):
            raise RuntimeError(
                "CPU affinity was requested, but this platform does not expose "
                "os.sched_setaffinity; rerun with --cpu-mode host"
            )

        def pin_child() -> None:
            os.sched_setaffinity(0, cpu_affinity)

        preexec_fn = pin_child

    proc = subprocess.run(
        command,
        cwd=cwd,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=preexec_fn,
    )
    return proc.stdout


def maybe_version(command: list[str]) -> str | None:
    try:
        return command_output(command).strip()
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def parse_records(output: str, expected_runtime: str) -> dict[str, dict[str, int]]:
    records: dict[str, dict[str, int]] = {}
    for match in RECORD_RE.finditer(output):
        runtime = match.group("runtime")
        if runtime != expected_runtime:
            continue
        name = match.group("benchmark")
        if name not in BENCHMARKS:
            continue
        records[name] = {
            "messages": int(match.group("messages")),
            "elapsed_ns": int(match.group("elapsed_ns")),
        }

    missing = [name for name in BENCHMARKS if name not in records]
    if missing:
        print(output, file=sys.stderr)
        raise RuntimeError(
            f"{expected_runtime} did not emit records for: {', '.join(missing)}"
        )
    return records


def build_commands(selected: list[str]) -> dict[str, list[str]]:
    BUILD.mkdir(parents=True, exist_ok=True)
    commands: dict[str, list[str]] = {}

    if "nulang" in selected:
        if shutil.which("cargo") is None:
            raise RuntimeError("cargo is required for the Nulang benchmark")
        # Build outside the affinity-constrained measured rounds so the first
        # runtime sample is not also paying compilation/startup work.
        command_output(
            ["cargo", "test", "--release", "--no-run", "benchmarks::bench_"]
        )
        commands["nulang"] = [
            "cargo",
            "test",
            "--release",
            "benchmarks::bench_",
            "--",
            "--nocapture",
            "--test-threads=1",
        ]

    if "rust" in selected:
        rustc = shutil.which("rustc")
        if rustc is None:
            raise RuntimeError("rustc is required for the Rust baseline")
        binary = BUILD / "rust_baseline"
        command_output(
            [
                rustc,
                "--edition=2021",
                "-O",
                str(FIXTURES / "rust_baseline.rs"),
                "-o",
                str(binary),
            ]
        )
        commands["rust"] = [str(binary)]

    if "go" in selected:
        go = shutil.which("go")
        if go is None:
            raise RuntimeError("go is required for the Go baseline")
        binary = BUILD / "go_baseline"
        command_output(
            [
                go,
                "build",
                "-trimpath",
                "-o",
                str(binary),
                str(FIXTURES / "go_baseline.go"),
            ]
        )
        commands["go"] = [str(binary)]

    if "erlang" in selected:
        erlc = shutil.which("erlc")
        erl = shutil.which("erl")
        if erlc is None or erl is None:
            raise RuntimeError("erl and erlc are required for the Erlang baseline")
        command_output(
            [
                erlc,
                "-o",
                str(BUILD),
                str(FIXTURES / "cross_runtime_baseline.erl"),
            ]
        )
        commands["erlang"] = [
            erl,
            "-noshell",
            "-pa",
            str(BUILD),
            "-s",
            "cross_runtime_baseline",
            "main",
            "-s",
            "init",
            "stop",
        ]

    if "pony" in selected:
        ponyc = shutil.which("ponyc")
        if ponyc is None:
            raise RuntimeError("ponyc is required for the Pony actor baseline")
        pony_build = BUILD / "pony"
        pony_build.mkdir(parents=True, exist_ok=True)
        command_output(
            [
                ponyc,
                "--output",
                str(pony_build),
                "--bin-name",
                "pony_baseline",
                str(FIXTURES / "pony_baseline"),
            ]
        )
        pony_binary = pony_build / (
            "pony_baseline.exe" if os.name == "nt" else "pony_baseline"
        )
        commands["pony"] = [str(pony_binary)]

    if "caf" in selected:
        cmake = shutil.which("cmake")
        if cmake is None:
            raise RuntimeError("cmake is required for the CAF actor baseline")
        caf_build = BUILD / "caf"
        command_output(
            [
                cmake,
                "-S",
                str(FIXTURES / "caf_baseline"),
                "-B",
                str(caf_build),
                "-DCMAKE_BUILD_TYPE=Release",
            ]
        )
        command_output(
            [cmake, "--build", str(caf_build), "--config", "Release", "--parallel"]
        )
        caf_binary = caf_build / (
            "Release/caf_baseline.exe" if os.name == "nt" else "caf_baseline"
        )
        commands["caf"] = [str(caf_binary)]

    return commands


def measurement_affinity(cpu_mode: str) -> set[int] | None:
    if cpu_mode == "host":
        return None

    if not hasattr(os, "sched_getaffinity") or not hasattr(os, "sched_setaffinity"):
        raise RuntimeError(
            "--cpu-mode single requires sched_getaffinity/sched_setaffinity; "
            "use --cpu-mode host on this platform"
        )

    allowed = sorted(os.sched_getaffinity(0))
    if not allowed:
        raise RuntimeError("the benchmark process has an empty CPU affinity set")
    return {allowed[0]}


def git_sha() -> str | None:
    return maybe_version(["git", "rev-parse", "HEAD"])


def environment_metadata(
    cpu_mode: str, cpu_affinity: set[int] | None
) -> dict[str, object]:
    cpu_model = None
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text(errors="replace").splitlines():
            if line.lower().startswith("model name") and ":" in line:
                cpu_model = line.split(":", 1)[1].strip()
                break

    allowed_cpus = None
    if hasattr(os, "sched_getaffinity"):
        allowed_cpus = sorted(os.sched_getaffinity(0))

    otp = None
    if shutil.which("erl"):
        otp = maybe_version(
            [
                "erl",
                "-noshell",
                "-eval",
                'io:format("~s", [erlang:system_info(otp_release)]), halt().',
            ]
        )

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "git_sha": git_sha(),
        "os": platform.platform(),
        "machine": platform.machine(),
        "cpu_count": os.cpu_count(),
        "cpu_model": cpu_model,
        "host_allowed_cpus": allowed_cpus,
        "measurement_cpu_mode": cpu_mode,
        "measurement_cpu_affinity": (
            sorted(cpu_affinity) if cpu_affinity is not None else None
        ),
        "nulang_shards": 1,
        "python": platform.python_version(),
        "rustc": maybe_version(["rustc", "--version"]) if shutil.which("rustc") else None,
        "cargo": maybe_version(["cargo", "--version"]) if shutil.which("cargo") else None,
        "go": maybe_version(["go", "version"]) if shutil.which("go") else None,
        "erlang_otp": otp,
        "ponyc": (
            maybe_version(["ponyc", "--version"]) if shutil.which("ponyc") else None
        ),
        "cmake": (
            maybe_version(["cmake", "--version"]) if shutil.which("cmake") else None
        ),
        "cxx": (
            maybe_version(["c++", "--version"]) if shutil.which("c++") else None
        ),
    }


def summarize(
    samples: dict[str, dict[str, list[dict[str, int]]]]
) -> dict[str, dict[str, dict[str, float | int]]]:
    summary: dict[str, dict[str, dict[str, float | int]]] = {}
    for runtime, workloads in samples.items():
        summary[runtime] = {}
        for name, rows in workloads.items():
            elapsed = [row["elapsed_ns"] for row in rows]
            message_counts = {row["messages"] for row in rows}
            if len(message_counts) != 1:
                raise RuntimeError(f"{runtime}/{name}: message count changed across samples")
            messages = message_counts.pop()
            median_ns = statistics.median(elapsed)
            summary[runtime][name] = {
                "samples": len(elapsed),
                "messages": messages,
                "median_elapsed_ns": int(median_ns),
                "min_elapsed_ns": min(elapsed),
                "max_elapsed_ns": max(elapsed),
                "median_ns_per_message": median_ns / messages,
                "median_messages_per_second": messages * 1_000_000_000 / median_ns,
            }
    return summary


def print_table(summary: dict[str, dict[str, dict[str, float | int]]]) -> None:
    print()
    print("runtime   benchmark      median msg/s   median ns/msg")
    print("--------  -------------  -------------  -------------")
    for runtime in sorted(summary):
        for name in BENCHMARKS:
            row = summary[runtime][name]
            print(
                f"{runtime:<8}  {name:<13}  "
                f"{row['median_messages_per_second']:>13,.0f}  "
                f"{row['median_ns_per_message']:>13,.1f}"
            )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=5, help="measured runs per runtime")
    parser.add_argument("--warmup", type=int, default=1, help="discarded warm-up runs")
    parser.add_argument(
        "--runtimes",
        default="nulang,rust,go,erlang",
        help="comma-separated subset of nulang,rust,go,erlang,pony,caf",
    )
    parser.add_argument(
        "--cpu-mode",
        choices=("single", "host"),
        default="single",
        help=(
            "single pins every measured runtime process to the same logical CPU "
            "(default, comparable to Nulang's single-shard harness); host leaves "
            "the host scheduler unconstrained and is diagnostic only"
        ),
    )
    parser.add_argument("--output", type=Path, help="write full JSON report here")
    args = parser.parse_args()

    if args.runs < 1 or args.warmup < 0:
        parser.error("--runs must be >= 1 and --warmup must be >= 0")

    selected = [item.strip() for item in args.runtimes.split(",") if item.strip()]
    unknown = sorted(
        set(selected) - {"nulang", "rust", "go", "erlang", "pony", "caf"}
    )
    if unknown:
        parser.error(f"unknown runtimes: {', '.join(unknown)}")

    cpu_affinity = measurement_affinity(args.cpu_mode)
    if cpu_affinity is not None:
        print(
            f"[topology] cpu_mode=single affinity={sorted(cpu_affinity)} "
            "nulang_shards=1",
            flush=True,
        )
    else:
        print(
            "[topology] cpu_mode=host affinity=unconstrained nulang_shards=1 "
            "(diagnostic; fork_join is not a fair multicore comparison)",
            flush=True,
        )

    commands = build_commands(selected)
    samples: dict[str, dict[str, list[dict[str, int]]]] = {
        runtime: {name: [] for name in BENCHMARKS} for runtime in selected
    }

    # Cycle across runtimes per sample rather than exhausting one runtime at a
    # time. This reduces simple thermal/load drift bias within a host run.
    total_rounds = args.warmup + args.runs
    for round_idx in range(total_rounds):
        measured = round_idx >= args.warmup
        phase = "sample" if measured else "warmup"
        index = round_idx - args.warmup + 1 if measured else round_idx + 1
        for runtime in selected:
            print(f"[{phase} {index}] {runtime}", flush=True)
            output = command_output(
                commands[runtime],
                cpu_affinity=cpu_affinity,
            )
            records = parse_records(output, runtime)
            if measured:
                for name, row in records.items():
                    samples[runtime][name].append(row)

    summary = summarize(samples)
    print_table(summary)

    report = {
        "schema": 2,
        "methodology": "matched Savina-style messaging baselines",
        "warmup_runs": args.warmup,
        "measured_runs": args.runs,
        "environment": environment_metadata(args.cpu_mode, cpu_affinity),
        "samples": samples,
        "summary": summary,
    }

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(f"wrote {args.output}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
