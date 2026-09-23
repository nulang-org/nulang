#!/usr/bin/env python3
"""Run matched Savina-style messaging baselines on one host.

The script intentionally compares standard runtime primitives, not third-party
actor frameworks. See benchmarks/cross_runtime/README.md before interpreting
or publishing results.
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


def command_output(command: list[str], *, cwd: Path = ROOT) -> str:
    proc = subprocess.run(
        command,
        cwd=cwd,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
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

    return commands


def git_sha() -> str | None:
    return maybe_version(["git", "rev-parse", "HEAD"])


def environment_metadata() -> dict[str, object]:
    cpu_model = None
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text(errors="replace").splitlines():
            if line.lower().startswith("model name") and ":" in line:
                cpu_model = line.split(":", 1)[1].strip()
                break

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
        "python": platform.python_version(),
        "rustc": maybe_version(["rustc", "--version"]) if shutil.which("rustc") else None,
        "cargo": maybe_version(["cargo", "--version"]) if shutil.which("cargo") else None,
        "go": maybe_version(["go", "version"]) if shutil.which("go") else None,
        "erlang_otp": otp,
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
        help="comma-separated subset of nulang,rust,go,erlang",
    )
    parser.add_argument("--output", type=Path, help="write full JSON report here")
    args = parser.parse_args()

    if args.runs < 1 or args.warmup < 0:
        parser.error("--runs must be >= 1 and --warmup must be >= 0")

    selected = [item.strip() for item in args.runtimes.split(",") if item.strip()]
    unknown = sorted(set(selected) - {"nulang", "rust", "go", "erlang"})
    if unknown:
        parser.error(f"unknown runtimes: {', '.join(unknown)}")

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
            output = command_output(commands[runtime])
            records = parse_records(output, runtime)
            if measured:
                for name, row in records.items():
                    samples[runtime][name].append(row)

    summary = summarize(samples)
    print_table(summary)

    report = {
        "schema": 1,
        "methodology": "standard-runtime Savina-style messaging baselines",
        "warmup_runs": args.warmup,
        "measured_runs": args.runs,
        "environment": environment_metadata(),
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
