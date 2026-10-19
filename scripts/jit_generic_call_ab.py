#!/usr/bin/env python3
"""Counterbalanced same-host Nulang generic-vs-concrete JIT call probe.

This is a diagnostic fixture, not a general language speedup benchmark.
It alternates ordering by round, verifies the benchmark runner's schema and
parity-enforcing output, and records both raw samples and medians. It must not
be used to infer statistical significance or to claim monomorphization exists.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parents[1]
CONCRETE = "concrete_identity_call_loop"
GENERIC = "generic_identity_call_loop"
METRICS = ("interpreter_run_ns", "first_run_ns", "warm_run_ns")
COMPILE_SECTIONS = ("first_compile", "warm_compile_delta")


def _unsigned_int(value: object, key: str, *, nonzero: bool) -> int:
    if type(value) is not int or value < (1 if nonzero else 0):
        raise ValueError(f"{key} must be a {'positive' if nonzero else 'nonnegative'} integer")
    return value


def validate_record(raw: dict, workload: str) -> dict:
    if not isinstance(raw, dict):
        raise ValueError("benchmark record must be a JSON object")
    if raw.get("schema") != 2 or raw.get("suite") != "jit-codegen":
        raise ValueError("unexpected benchmark schema/suite")
    if raw.get("workload") != workload:
        raise ValueError(f"unexpected workload: {raw.get('workload')!r}; expected {workload}")
    if raw.get("iteration") != 1:
        raise ValueError("expected exactly one iteration per benchmark process")
    for metric in METRICS:
        _unsigned_int(raw.get(metric), metric, nonzero=True)
    for section in COMPILE_SECTIONS:
        block = raw.get(section)
        if not isinstance(block, dict):
            raise ValueError(f"{section} missing from benchmark output")
        _unsigned_int(block.get("total_compile_ns"), section + ".total_compile_ns", nonzero=False)
    return raw


def parse_single_record(stdout: str, workload: str) -> dict:
    lines = [line for line in stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        raise ValueError(f"expected exactly one benchmark JSONL record, got {len(lines)}")
    try:
        record = json.loads(lines[0])
    except json.JSONDecodeError as exc:
        raise ValueError("benchmark runner did not emit valid JSONL") from exc
    return validate_record(record, workload)


def find_benchmark_executable(stdout: str) -> Path:
    """Use Cargo's build messages, never assume a hard-coded target dir."""
    executable = None
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        if (event.get("reason") == "compiler-artifact"
                and event.get("target", {}).get("name") == "nulang-jit-bench"):
            if event.get("executable"):
                executable = Path(event["executable"])
    if executable is None:
        raise ValueError("Cargo build emitted no nulang-jit-bench executable")
    return executable


def build_benchmark() -> Path:
    """Compile once without CPU pinning; avoid Cargo invocations between pairs."""
    command = [
        "cargo", "build", "--locked", "--release", "--no-default-features",
        "--features", "native-codegen", "--bin", "nulang-jit-bench",
        "--message-format=json",
    ]
    process = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
    if process.returncode != 0:
        raise RuntimeError(
            f"benchmark build failed with exit {process.returncode}:\n{process.stderr[-4000:]}"
        )
    executable = find_benchmark_executable(process.stdout)
    if not executable.is_file():
        raise RuntimeError(f"Cargo-reported benchmark executable missing: {executable}")
    return executable


def select_affinity(mode: str, available: set[int]) -> set[int] | None:
    if mode == "host":
        return None
    if mode != "single":
        raise ValueError(f"unknown CPU mode: {mode}")
    if not available:
        raise ValueError("empty CPU affinity set")
    return {min(available)}


def measurement_affinity(mode: str) -> set[int] | None:
    if mode == "host":
        return None
    if not hasattr(os, "sched_getaffinity") or not hasattr(os, "sched_setaffinity"):
        raise ValueError("single CPU mode requires Linux CPU affinity support")
    return select_affinity(mode, os.sched_getaffinity(0))


def run_benchmark(workload: str, executable: Path, affinity: set[int] | None) -> dict:
    command = [str(executable), "--workload", workload, "--format", "jsonl", "--repeat", "1"]
    # Apply pinning only to the measured process, not the parallel Cargo build.
    pin = None if affinity is None else lambda: os.sched_setaffinity(0, affinity)
    process = subprocess.run(
        command, cwd=ROOT, capture_output=True, text=True, check=False, preexec_fn=pin
    )
    if process.returncode != 0:
        raise RuntimeError(
            f"benchmark {workload} failed with exit {process.returncode}:\n"
            f"{process.stderr[-4000:]}"
        )
    return parse_single_record(process.stdout, workload)


def measure_pairs(rounds: int, runner: Callable[[str], dict]) -> tuple[dict, list[dict]]:
    if type(rounds) is not int or not 5 <= rounds <= 100:
        raise ValueError("paired comparisons require at least 5 and at most 100 rounds")

    results = {CONCRETE: [], GENERIC: []}
    samples: list[dict] = []
    warm_ratios: list[float] = []
    for index in range(rounds):
        order = (CONCRETE, GENERIC) if index % 2 == 0 else (GENERIC, CONCRETE)
        paired = {}
        for workload in order:
            entry = validate_record(runner(workload), workload)
            paired[workload] = entry
            results[workload].append(entry)
        warm_ratios.append(
            paired[GENERIC]["warm_run_ns"] / paired[CONCRETE]["warm_run_ns"]
        )
        samples.append({"round": index + 1, "order": list(order), "records": paired})

    summary = {
        "schema": 1,
        "comparison": "erased-generic-vs-concrete-identity-call",
        "rounds": rounds,
        "observational_only": True,
        "ordering": "alternating concrete-first/generic-first",
        "median_warm_generic_to_concrete_ratio": statistics.median(warm_ratios),
        "median_warm_ns": {
            workload: statistics.median(row["warm_run_ns"] for row in results[workload])
            for workload in (CONCRETE, GENERIC)
        },
        "median_interpreter_ns": {
            workload: statistics.median(row["interpreter_run_ns"] for row in results[workload])
            for workload in (CONCRETE, GENERIC)
        },
        "median_first_run_ns": {
            workload: statistics.median(row["first_run_ns"] for row in results[workload])
            for workload in (CONCRETE, GENERIC)
        },
        "median_first_compile_ns": {
            workload: statistics.median(row["first_compile"]["total_compile_ns"] for row in results[workload])
            for workload in (CONCRETE, GENERIC)
        },
        "median_warm_compile_delta_ns": {
            workload: statistics.median(row["warm_compile_delta"]["total_compile_ns"] for row in results[workload])
            for workload in (CONCRETE, GENERIC)
        },
    }
    return summary, samples


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=8)
    parser.add_argument("--cpu-mode", choices=("single", "host"), default="single")
    parser.add_argument("--output", type=Path, default=Path("jit-generic-call-ab.json"))
    args = parser.parse_args()
    try:
        affinity = measurement_affinity(args.cpu_mode)
        executable = build_benchmark()
        summary, samples = measure_pairs(
            args.rounds, lambda workload: run_benchmark(workload, executable, affinity)
        )
        summary["cpu_mode"] = args.cpu_mode
        summary["measured_cpu_affinity"] = sorted(affinity) if affinity is not None else None
    except (ValueError, RuntimeError, OSError) as exc:
        parser.error(str(exc))
    report = {"summary": summary, "samples": samples}
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(summary, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
