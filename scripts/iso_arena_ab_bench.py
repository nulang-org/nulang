#!/usr/bin/env python3
"""Paired exact-base Criterion A/B for Nulang's lazy actor iso arena.

The *same* benchmark source from the candidate is installed into a temporary
base worktree. Production source (src/iso_arena.rs) is NOT changed in that
worktree. Base/candidate binaries are built separately, then interleaved on
one pinned CPU. This isolates active-arena behavior absent from the generic
actor-density probe. No uploads or repo writes occur outside temp worktrees.
"""
from __future__ import annotations

import argparse
import json
import math
import os
import pathlib
import random
import shutil
import statistics
import subprocess
import tempfile
from datetime import datetime, timezone

ROOT = pathlib.Path(__file__).resolve().parents[1]
CASES = ("idle_reset_128", "active_alloc_reset_128")


def run(command: list[str], *, cwd: pathlib.Path, env: dict[str, str] | None = None) -> str:
    result = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(
            f"command failed ({result.returncode}): {command!r}\n"
            + result.stdout[-2500:] + "\n" + result.stderr[-5000:]
        )
    return result.stdout


def read_median_ns(path: pathlib.Path) -> float:
    value = json.loads(path.read_text())["median"]["point_estimate"]
    if not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
        raise ValueError(f"invalid Criterion median: {value!r}")
    return float(value)


def variant_order(round_index: int) -> tuple[str, str]:
    return ("base", "candidate") if round_index % 2 == 0 else ("candidate", "base")


def paired_reduction(base: list[float], candidate: list[float]) -> dict[str, object]:
    if not base or len(base) != len(candidate):
        raise ValueError("paired base/candidate samples must be nonempty and equal length")
    if not all(math.isfinite(x) and x > 0 for x in base + candidate):
        raise ValueError("paired measurements must be positive and finite")
    deltas = [100.0 * (b - c) / b for b, c in zip(base, candidate)]
    rng = random.Random(0x150A_B)
    n = len(deltas)
    boot = sorted(statistics.median(deltas[rng.randrange(n)] for _ in range(n))
                  for _ in range(4_000))
    return {
        "pairs": n,
        "base_median_ns_per_128": statistics.median(base),
        "candidate_median_ns_per_128": statistics.median(candidate),
        "median_reduction_pct": statistics.median(deltas),
        "ci95_reduction_pct": [boot[99], boot[3899]],
        "paired_reductions_pct": deltas,
    }


def compile_bench(root: pathlib.Path, target: pathlib.Path) -> pathlib.Path:
    env = dict(os.environ, CARGO_TARGET_DIR=str(target),
               CARGO_INCREMENTAL="0", CARGO_TERM_COLOR="never")
    stdout = run(
        ["cargo", "bench", "--locked", "--no-default-features", "--bench",
         "bench_main", "--no-run", "--message-format=json"],
        cwd=root, env=env,
    )
    binaries: set[str] = set()
    for line in stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        tgt = message.get("target") or {}
        if message.get("reason") == "compiler-artifact" and tgt.get("name") == "bench_main":
            path = message.get("executable")
            if path:
                binaries.add(path)
    if len(binaries) != 1:
        raise RuntimeError(f"expected one Criterion bench_main executable; found {binaries}")
    return pathlib.Path(next(iter(binaries)))


def criterion_estimate_path(home: pathlib.Path, case: str) -> pathlib.Path:
    # Criterion's BenchmarkId::make_filename_safe replaces '/' in a group
    # with '_'. The function ID is then placed in its own subdirectory.
    return home / "actor_iso_arena" / case / "new" / "estimates.json"


def criterion_case(binary: pathlib.Path, target: pathlib.Path, case: str,
                   cpu: int | None, warmup_seconds: float,
                   measurement_seconds: float) -> float:
    name = f"actor/iso_arena/{case}"
    cmd = [str(binary), "--bench", name, "--sample-size", "20",
           "--warm-up-time", str(warmup_seconds),
           "--measurement-time", str(measurement_seconds), "--noplot"]
    if cpu is not None and shutil.which("taskset"):
        cmd = ["taskset", "-c", str(cpu)] + cmd
    # The Criterion binary is launched directly, rather than through Cargo.
    # Give each build an explicit output home so Criterion's cargo-metadata
    # fallback cannot place both variants' measurements in the checkout.
    criterion_home = target / "criterion"
    criterion_home.mkdir(parents=True, exist_ok=True)
    estimate = criterion_estimate_path(criterion_home, case)
    # Prevent a no-op/filter mismatch from silently reusing a prior sample.
    estimate.unlink(missing_ok=True)
    output = run(
        cmd,
        cwd=ROOT,
        env=dict(os.environ, CARGO_TARGET_DIR=str(target),
                 CRITERION_HOME=str(criterion_home), CARGO_TERM_COLOR="never"),
    )
    if not estimate.is_file():
        alternatives = sorted(str(p.relative_to(criterion_home))
                              for p in criterion_home.rglob("estimates.json"))
        raise RuntimeError(
            f"Criterion did not produce {estimate} for {name}. "
            f"Other estimates: {alternatives[:12]!r}. "
            f"Benchmark stdout tail: {output[-1500:]!r}"
        )
    return read_median_ns(estimate)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--pairs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--warmup-seconds", type=float, default=1.0)
    parser.add_argument("--measurement-seconds", type=float, default=2.0)
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path("iso-arena-ab.json"))
    args = parser.parse_args()
    if args.pairs < 2 or args.warmup < 0 or args.warmup_seconds <= 0 or args.measurement_seconds <= 0:
        parser.error("invalid round counts or Criterion timings")

    base_sha = run(["git", "rev-parse", f"{args.base_ref}^{{commit}}"], cwd=ROOT).strip()
    candidate_sha = run(["git", "rev-parse", "HEAD"], cwd=ROOT).strip()
    cpu = min(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None
    samples: dict[str, dict[str, list[float]]] = {
        label: {case: [] for case in CASES} for label in ("base", "candidate")
    }
    with tempfile.TemporaryDirectory(prefix="nulang-iso-ab-") as td:
        temporary = pathlib.Path(td)
        base = temporary / "base"
        targets = {v: temporary / "target" / v for v in samples}
        run(["git", "worktree", "add", "--detach", str(base), base_sha], cwd=ROOT)
        try:
            bench_source = ROOT / "benches" / "actor_bench.rs"
            if "fn bench_iso_arena(" not in bench_source.read_text():
                raise RuntimeError("candidate must contain bench_iso_arena")
            # Exact same measurement code. Only production library code varies.
            shutil.copyfile(bench_source, base / "benches" / "actor_bench.rs")
            roots = {"base": base, "candidate": ROOT}
            binaries = {}
            for variant in ("base", "candidate"):
                print(f"[build] {variant}", flush=True)
                binaries[variant] = compile_bench(roots[variant], targets[variant])
            for round_index in range(args.warmup + args.pairs):
                measured = round_index >= args.warmup
                for case in CASES:
                    for variant in variant_order(round_index):
                        print(f"[{'sample' if measured else 'warmup'} {round_index + 1}] "
                              f"{variant}/{case}", flush=True)
                        value = criterion_case(
                            binaries[variant], targets[variant], case, cpu,
                            args.warmup_seconds, args.measurement_seconds,
                        )
                        if measured:
                            samples[variant][case].append(value)
            comparison = {
                case: paired_reduction(samples["base"][case], samples["candidate"][case])
                for case in CASES
            }
            result = {
                "schema": 1,
                "base_sha": base_sha,
                "candidate_sha": candidate_sha,
                "method": "paired same-host Criterion median ns per 128 operations",
                "benchmark_source": "candidate benches/actor_bench.rs copied into temporary base",
                "machine_cpu_affinity": cpu,
                "timestamp_utc": datetime.now(timezone.utc).isoformat(),
                "cases": comparison,
            }
            args.output.write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result, indent=2), flush=True)
        finally:
            run(["git", "worktree", "remove", "--force", str(base)], cwd=ROOT)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
