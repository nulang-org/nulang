#!/usr/bin/env python3
"""Run matched steady-state remote actor round-trip baselines on one host.

Each runtime measures one scalar request hop over loopback TCP, receiver actor
scheduling/handler work, the return hop, and sender-side actor dispatch. Process
startup, listener setup, connection establishment, and workload warm-up happen
outside the timed section in each runtime-specific runner.

The wire encodings are intentionally runtime-native rather than byte-identical;
this is a runtime-stack comparison, not a packet-codec comparison. Use the NUL0
codec Criterion benchmarks when isolating serialization itself.
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
FIXTURES = ROOT / "benchmarks" / "remote_roundtrip"
BUILD = ROOT / "target" / "remote-roundtrip"
RUNTIMES = ("nulang", "go", "erlang")
RECORD_RE = re.compile(
    r"\[remote-roundtrip\]\s+runtime=(?P<runtime>[a-z0-9_-]+)\s+"
    r"iteration=(?P<iteration>\d+)\s+"
    r"roundtrips=(?P<roundtrips>\d+)\s+"
    r"elapsed_ns=(?P<elapsed_ns>\d+)\s+"
    r"ns_per_roundtrip=(?P<ns_per_roundtrip>[0-9.]+)"
)


def command_output(
    command: list[str],
    *,
    cwd: Path = ROOT,
    cpu_affinity: set[int] | None = None,
    env: dict[str, str] | None = None,
) -> str:
    preexec_fn = None
    if cpu_affinity is not None:
        if not hasattr(os, "sched_setaffinity"):
            raise RuntimeError(
                "CPU affinity was requested but os.sched_setaffinity is unavailable; "
                "rerun with --cpu-mode host"
            )

        def pin_child() -> None:
            os.sched_setaffinity(0, cpu_affinity)

        preexec_fn = pin_child

    child_env = os.environ.copy()
    if env:
        child_env.update(env)
    proc = subprocess.run(
        command,
        cwd=cwd,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=preexec_fn,
        env=child_env,
    )
    return proc.stdout


def maybe_version(command: list[str]) -> str | None:
    try:
        return command_output(command).strip()
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def parse_record(output: str, expected_runtime: str, expected_roundtrips: int) -> dict[str, int]:
    matches = [
        match
        for match in RECORD_RE.finditer(output)
        if match.group("runtime") == expected_runtime
    ]
    if len(matches) != 1:
        print(output, file=sys.stderr)
        raise RuntimeError(
            f"{expected_runtime} emitted {len(matches)} remote-roundtrip records; expected 1"
        )
    match = matches[0]
    roundtrips = int(match.group("roundtrips"))
    if roundtrips != expected_roundtrips:
        raise RuntimeError(
            f"{expected_runtime} roundtrip count changed: {roundtrips} != {expected_roundtrips}"
        )
    return {
        "roundtrips": roundtrips,
        "elapsed_ns": int(match.group("elapsed_ns")),
    }


def runtime_order(runtimes: list[str], round_index: int) -> list[str]:
    return list(runtimes if round_index % 2 == 0 else reversed(runtimes))


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
        raise RuntimeError("benchmark process has an empty CPU affinity set")
    return {allowed[0]}


def build_commands(selected: list[str], roundtrips: int, warmup: int) -> dict[str, list[str]]:
    BUILD.mkdir(parents=True, exist_ok=True)
    commands: dict[str, list[str]] = {}

    if "nulang" in selected:
        if shutil.which("cargo") is None:
            raise RuntimeError("cargo is required for the Nulang remote-roundtrip benchmark")
        command_output(
            [
                "cargo",
                "build",
                "--locked",
                "--profile",
                "savina",
                "--no-default-features",
                "--features",
                "tcp",
                "--bin",
                "nulang-remote-roundtrip",
            ]
        )
        metadata = json.loads(
            command_output(["cargo", "metadata", "--format-version", "1", "--no-deps"])
        )
        executable = "nulang-remote-roundtrip.exe" if os.name == "nt" else "nulang-remote-roundtrip"
        binary = Path(metadata["target_directory"]) / "savina" / executable
        commands["nulang"] = [
            str(binary),
            "--roundtrips",
            str(roundtrips),
            "--warmup",
            str(warmup),
            "--format",
            "human",
        ]

    if "go" in selected:
        go = shutil.which("go")
        if go is None:
            raise RuntimeError("go is required for the Go remote-roundtrip baseline")
        binary = BUILD / ("go_baseline.exe" if os.name == "nt" else "go_baseline")
        command_output(
            [go, "build", "-trimpath", "-o", str(binary), str(FIXTURES / "go_baseline.go")]
        )
        commands["go"] = [
            str(binary),
            "--roundtrips",
            str(roundtrips),
            "--warmup",
            str(warmup),
        ]

    if "erlang" in selected:
        escript = shutil.which("escript")
        if escript is None:
            raise RuntimeError("escript is required for the Erlang remote-roundtrip baseline")
        commands["erlang"] = [
            escript,
            str(FIXTURES / "erlang_baseline.escript"),
            "--roundtrips",
            str(roundtrips),
            "--warmup",
            str(warmup),
        ]

    return commands


def runtime_env(runtime: str, cpu_mode: str) -> dict[str, str]:
    if cpu_mode != "single":
        return {}
    if runtime == "go":
        return {"GOMAXPROCS": "1"}
    if runtime == "erlang":
        # Keep BEAM's scheduler budget aligned with the one-logical-CPU process
        # affinity used for the other runtimes. Dirty schedulers remain runtime
        # implementation detail but inherit the same CPU affinity mask.
        return {"ERL_FLAGS": "+S 1:1"}
    return {}


def summarize(samples: dict[str, list[dict[str, int]]]) -> dict[str, dict[str, float | int]]:
    summary: dict[str, dict[str, float | int]] = {}
    for runtime, rows in samples.items():
        elapsed = [row["elapsed_ns"] for row in rows]
        counts = {row["roundtrips"] for row in rows}
        if len(counts) != 1:
            raise RuntimeError(f"{runtime}: roundtrip count changed across samples")
        roundtrips = counts.pop()
        median_elapsed = statistics.median(elapsed)
        summary[runtime] = {
            "samples": len(rows),
            "roundtrips": roundtrips,
            "median_elapsed_ns": int(median_elapsed),
            "min_elapsed_ns": min(elapsed),
            "max_elapsed_ns": max(elapsed),
            "median_ns_per_roundtrip": median_elapsed / roundtrips,
            "median_roundtrips_per_second": roundtrips * 1_000_000_000 / median_elapsed,
        }
    return summary


def environment_metadata(cpu_mode: str, affinity: set[int] | None) -> dict[str, object]:
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
        "git_sha": maybe_version(["git", "rev-parse", "HEAD"]),
        "os": platform.platform(),
        "machine": platform.machine(),
        "cpu_count": os.cpu_count(),
        "cpu_model": cpu_model,
        "cpu_mode": cpu_mode,
        "measurement_cpu_affinity": sorted(affinity) if affinity is not None else None,
        "rustc": maybe_version(["rustc", "--version"]) if shutil.which("rustc") else None,
        "go": maybe_version(["go", "version"]) if shutil.which("go") else None,
        "erlang_otp": otp,
        "python": platform.python_version(),
    }


def print_table(summary: dict[str, dict[str, float | int]]) -> None:
    print()
    print("runtime   median rtt/s   median ns/roundtrip")
    print("--------  -------------  -------------------")
    for runtime in RUNTIMES:
        if runtime not in summary:
            continue
        row = summary[runtime]
        print(
            f"{runtime:<8}  {row['median_roundtrips_per_second']:>13,.0f}  "
            f"{row['median_ns_per_roundtrip']:>19,.1f}"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=8, help="measured process runs per runtime")
    parser.add_argument("--roundtrips", type=int, default=10_000, help="measured round trips per run")
    parser.add_argument("--warmup", type=int, default=1_000, help="untimed round trips inside each run")
    parser.add_argument(
        "--runtimes",
        default="nulang,go,erlang",
        help="comma-separated subset of nulang,go,erlang",
    )
    parser.add_argument(
        "--cpu-mode",
        choices=("single", "host"),
        default="single",
        help="single pins every runtime process to one logical CPU; host is diagnostic",
    )
    parser.add_argument("--output", type=Path, help="write the full JSON report")
    args = parser.parse_args()

    if args.runs < 1 or args.roundtrips < 1 or args.warmup < 0:
        parser.error("--runs and --roundtrips must be >= 1; --warmup must be >= 0")

    selected = [item.strip() for item in args.runtimes.split(",") if item.strip()]
    unknown = sorted(set(selected) - set(RUNTIMES))
    if unknown:
        parser.error(f"unknown runtimes: {', '.join(unknown)}")
    if len(selected) != len(set(selected)):
        parser.error("runtime list contains duplicates")

    affinity = measurement_affinity(args.cpu_mode)
    commands = build_commands(selected, args.roundtrips, args.warmup)
    samples: dict[str, list[dict[str, int]]] = {runtime: [] for runtime in selected}

    for round_index in range(args.runs):
        order = runtime_order(selected, round_index)
        for runtime in order:
            print(f"[sample {round_index + 1}/{args.runs}] {runtime}", flush=True)
            output = command_output(
                commands[runtime],
                cpu_affinity=affinity,
                env=runtime_env(runtime, args.cpu_mode),
            )
            samples[runtime].append(parse_record(output, runtime, args.roundtrips))

    summary = summarize(samples)
    print_table(summary)
    report = {
        "schema": 1,
        "methodology": "matched steady-state loopback TCP actor round trip",
        "runs": args.runs,
        "roundtrips_per_run": args.roundtrips,
        "warmup_roundtrips": args.warmup,
        "environment": environment_metadata(args.cpu_mode, affinity),
        "samples": samples,
        "summary": summary,
        "interpretation": {
            "operation": "one scalar request hop + receiver actor dispatch + return hop + sender actor dispatch",
            "excluded": ["process startup", "listener setup", "connection establishment", "warm-up"],
            "wire_protocols_are_runtime_native": True,
        },
    }

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(f"wrote {args.output}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
