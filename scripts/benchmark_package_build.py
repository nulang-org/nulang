#!/usr/bin/env python3
"""Compare Nulang CLI build latency using already-built base/candidate binaries.

Pairs alternate AB/BA execution order on one host. Each variant builds a fresh
copy of the same package, then repeats the build in the same copy. Cargo build,
source copying, and fixture setup are excluded from timings. This measures
*workflow wall clock* (including compiler startup and dependency resolution),
not compiler-stage time or cold OS filesystem cache performance.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path


class BenchmarkError(RuntimeError):
    pass


def alternating_order(round_index: int) -> tuple[str, str]:
    return ("base", "candidate") if round_index % 2 == 0 else ("candidate", "base")


def digest_artifacts(project: Path) -> dict[str, str]:
    dist = project / ".nula" / "dist"
    if not dist.is_dir():
        raise BenchmarkError(f"missing artifact directory: {dist}")
    paths = sorted([*dist.glob("*.nbc"), *dist.glob("*.behavior.json")])
    if not any(p.suffix == ".nbc" for p in paths) or not any(
        p.name.endswith(".behavior.json") for p in paths
    ):
        raise BenchmarkError(f"missing bytecode or behavior manifest in {dist}")
    return {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}


def run_build(binary: Path, project: Path, mode: str, env: dict[str, str], timeout_seconds: float) -> int:
    args = [str(binary), "nula", "build"]
    if mode == "json":
        args.append("--json")
    start_ns = time.perf_counter_ns()
    try:
        proc = subprocess.run(
            args, cwd=project, env=env, capture_output=True, text=True,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as exc:
        raise BenchmarkError(
            f"compiler timed out after {timeout_seconds:g} seconds: {' '.join(args)}"
        ) from exc
    duration_ns = time.perf_counter_ns() - start_ns
    if proc.returncode != 0:
        raise BenchmarkError(
            f"compiler exited {proc.returncode}: {' '.join(args)}\n"
            f"stdout: {proc.stdout[-2500:]}\nstderr: {proc.stderr[-2500:]}"
        )
    if mode == "json":
        try:
            report = json.loads(proc.stdout)
        except json.JSONDecodeError as exc:
            raise BenchmarkError(f"non-JSON build output: {proc.stdout[-1000:]}") from exc
        if not (
            report.get("schema_version") == 1
            and report.get("command") == "build"
            and report.get("ok") is True
            and isinstance(report.get("diagnostics"), list)
        ):
            raise BenchmarkError(f"unexpected JSON build report: {report}")
    return duration_ns


def benchmark_packages(
    base_binary: Path,
    candidate_binary: Path,
    package: Path,
    *,
    rounds: int = 7,
    modes: tuple[str, ...] = ("normal", "json"),
    stdlib: Path | None = None,
    timeout_seconds: float = 120.0,
) -> dict:
    if rounds < 1:
        raise BenchmarkError("rounds must be >= 1")
    if not (0 < timeout_seconds <= 86_400):
        raise BenchmarkError("timeout_seconds must be within (0, 86400]")
    if not (package / "Nulang.toml").is_file():
        raise BenchmarkError("fixture must have Nulang.toml")
    if not (package / "src").is_dir():
        raise BenchmarkError("fixture must have src directory")
    for binary in (base_binary, candidate_binary):
        if not binary.is_file():
            raise BenchmarkError(f"missing compiler executable: {binary}")
    if not modes or any(mode not in ("normal", "json") for mode in modes):
        raise BenchmarkError("modes must contain normal and/or json")
    binaries = {"base": base_binary.resolve(), "candidate": candidate_binary.resolve()}
    env = os.environ.copy()
    # Prevent inherited package-module resolution from changing the fixture.
    env.pop("NULANG_MODULE_PATH", None)
    if stdlib is not None:
        if not stdlib.is_dir():
            raise BenchmarkError(f"missing stdlib: {stdlib}")
        env["NULANG_STDLIB"] = str(stdlib.resolve())
    samples: dict[str, dict[str, dict[str, list[int]]]] = {
        mode: {
            phase: {variant: [] for variant in binaries}
            for phase in ("fresh", "repeat")
        }
        for mode in modes
    }
    for mode in modes:
        for index in range(rounds):
            with tempfile.TemporaryDirectory(prefix="nulang-cli-bench-") as tmp:
                root = Path(tmp)
                signatures = {}
                for variant in alternating_order(index):
                    project = root / variant
                    shutil.copytree(
                        package,
                        project,
                        ignore=shutil.ignore_patterns(".nula", "dist", ".git", "target"),
                    )
                    samples[mode]["fresh"][variant].append(
                        run_build(binaries[variant], project, mode, env, timeout_seconds)
                    )
                    first = digest_artifacts(project)
                    samples[mode]["repeat"][variant].append(
                        run_build(binaries[variant], project, mode, env, timeout_seconds)
                    )
                    second = digest_artifacts(project)
                    if first != second:
                        raise BenchmarkError(f"artifact mismatch between fresh and repeat: {variant}/{mode}")
                    signatures[variant] = second
                if signatures["base"] != signatures["candidate"]:
                    raise BenchmarkError(
                        f"artifact mismatch between base and candidate ({mode}, round {index})\n"
                        f"base={signatures['base']}\ncandidate={signatures['candidate']}"
                    )
    # Pair the ratios by original round number, independent of AB/BA order.
    paired = {}
    for mode in modes:
        paired[mode] = {}
        for phase in ("fresh", "repeat"):
            base = samples[mode][phase]["base"]
            candidate = samples[mode][phase]["candidate"]
            paired[mode][phase] = statistics.median(b / c for b, c in zip(base, candidate))
    return {
        "schema_version": 1,
        "rounds": rounds,
        "host": {"platform": platform.platform(), "python": platform.python_version()},
        "binaries_sha256": {
            name: hashlib.sha256(path.read_bytes()).hexdigest()
            for name, path in binaries.items()
        },
        "samples_ns": samples,
        "paired_speedup_x": paired,
        "scope": "subprocess wall time; fresh and repeat package workspaces; not OS cold cache",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-binary", required=True, type=Path)
    parser.add_argument("--candidate-binary", required=True, type=Path)
    parser.add_argument("--package", required=True, type=Path)
    parser.add_argument("--stdlib", type=Path)
    parser.add_argument("--rounds", type=int, default=7)
    parser.add_argument("--timeout-seconds", type=float, default=120.0)
    parser.add_argument("--mode", choices=("normal", "json", "both"), default="both")
    parser.add_argument("--out", type=Path, help="write JSON output to this file instead of stdout")
    args = parser.parse_args()
    modes = ("normal", "json") if args.mode == "both" else (args.mode,)
    try:
        result = benchmark_packages(
            args.base_binary, args.candidate_binary, args.package,
            rounds=args.rounds, modes=modes, stdlib=args.stdlib,
            timeout_seconds=args.timeout_seconds,
        )
    except BenchmarkError as exc:
        parser.exit(1, f"benchmark invalid: {exc}\n")
    output = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.out:
        args.out.write_text(output)
    else:
        print(output, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
