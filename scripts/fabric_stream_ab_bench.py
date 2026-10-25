#!/usr/bin/env python3
"""Paired, exact-base Criterion A/B for segmented Fabric stream tail reads.

The benchmark fixture is intentionally overlaid byte-for-byte onto the base
worktree only after proving that removing it reproduces the base source.
This avoids comparing a candidate-only benchmark to nonexistent base data.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BENCH_FILE = Path("benches/persist_bench.rs")
COUNTS = (8, 128, 1024)
BENCHMARK_GROUP = "persist/fabric_tail_read"
# Criterion replaces group-name path separators in report directory names.
CRITERION_GROUP_DIR = BENCHMARK_GROUP.replace("/", "_")
FIXTURE_START = "/// Warm tail reads with increasingly long on-disk segment histories."
GROUP = "criterion_group!(\n    benches,"
BASE_IMPORTS = "use std::collections::HashMap;\n\n"
CANDIDATE_IMPORTS = (
    "use std::collections::HashMap;\n"
    "use std::fs;\n"
    "use std::sync::atomic::{AtomicUsize, Ordering};\n\n"
    "use nulang::runtime::{FabricStreamConfig, FileFabricStreamStore};\n\n"
)
BASE_GROUP_TAIL = "    bench_memory_journal_read\n);"
CANDIDATE_GROUP_TAIL = (
    "    bench_memory_journal_read,\n    bench_fabric_tail_read\n);"
)


def matched_fixture(base: str, candidate: str) -> str:
    """Reject every unrelated benchmark change before overlaying the fixture."""
    if (
        candidate.count(FIXTURE_START) != 1
        or candidate.count(CANDIDATE_IMPORTS) != 1
        or candidate.count(CANDIDATE_GROUP_TAIL) != 1
    ):
        raise ValueError("expected exactly one Fabric stream fixture, imports, and group entry")
    begin = candidate.index(FIXTURE_START)
    end = candidate.find(GROUP, begin)
    if end < 0:
        raise ValueError("Fabric stream fixture lacks benchmark group")
    stripped = candidate[:begin] + candidate[end:]
    stripped = stripped.replace(CANDIDATE_IMPORTS, BASE_IMPORTS, 1)
    stripped = stripped.replace(CANDIDATE_GROUP_TAIL, BASE_GROUP_TAIL, 1)
    if stripped != base:
        raise ValueError("base and candidate benchmark bodies differ by unrelated changes")
    return candidate

def read_estimates(target_dir: Path) -> dict[str, float]:
    results: dict[str, float] = {}
    for count in COUNTS:
        path = (
            target_dir / "criterion" / CRITERION_GROUP_DIR / str(count)
            / "new" / "estimates.json"
        )
        if not path.is_file():
            raise ValueError(f"Criterion report missing: {path}")
        payload = json.loads(path.read_text())
        value = float(payload["mean"]["point_estimate"])
        if not math.isfinite(value) or value <= 0:
            raise ValueError(f"invalid Criterion mean for {count} segments")
        results[str(count)] = value
    return results


def summarize_pairs(base: list[float], candidate: list[float]) -> dict[str, float | int]:
    if len(base) != len(candidate) or not base:
        raise ValueError("unequal or empty paired samples")
    if any(not math.isfinite(x) or x <= 0 for x in base + candidate):
        raise ValueError("invalid nonpositive or nonfinite paired timing")
    speedups = [b / c for b, c in zip(base, candidate)]
    return {
        "pairs": len(speedups),
        "median_base_ns": statistics.median(base),
        "median_candidate_ns": statistics.median(candidate),
        "median_speedup_x": statistics.median(speedups),
        "median_latency_change_pct": statistics.median(
            (c / b - 1) * 100 for b, c in zip(base, candidate)
        ),
    }


def execute(command: list[str], cwd: Path, env: dict[str, str] | None = None,
            pin_cpu: int | None = None) -> str:
    def pin() -> None:
        if pin_cpu is not None:
            os.sched_setaffinity(0, {pin_cpu})
    proc = subprocess.run(
        command, cwd=cwd, env=env, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        preexec_fn=pin if pin_cpu is not None else None, check=False,
    )
    if proc.returncode:
        sys.stderr.write(proc.stdout)
        raise subprocess.CalledProcessError(proc.returncode, command, output=proc.stdout)
    return proc.stdout


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--runs", type=int, default=8)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--output", type=Path, default=Path("fabric-stream-ab.json"))
    args = parser.parse_args()
    if args.runs < 2 or args.warmup < 0:
        parser.error("--runs must be >=2 and --warmup >=0")
    if not hasattr(os, "sched_getaffinity") or not hasattr(os, "sched_setaffinity"):
        raise ValueError("requires Linux CPU affinity")
    allowed = sorted(os.sched_getaffinity(0))
    if not allowed:
        raise ValueError("no allowed CPU")
    cpu = allowed[0]

    root = Path(tempfile.mkdtemp(prefix="fabric-stream-ab-"))
    base = root / "base"
    created = False
    try:
        execute(["git", "worktree", "add", "--detach", str(base), args.base_ref], ROOT)
        created = True
        candidate_content = (ROOT / BENCH_FILE).read_text()
        base_path = base / BENCH_FILE
        fixture = matched_fixture(base_path.read_text(), candidate_content)
        base_path.write_text(fixture)

        variants = {"base": base, "candidate": ROOT}
        envs = {}
        for name, path in variants.items():
            env = os.environ.copy()
            env["CARGO_TARGET_DIR"] = str(root / ("target-" + name))
            envs[name] = env
            print(f"[build] {name}", flush=True)
            execute(
                ["cargo", "bench", "--locked", "--no-default-features",
                 "--bench", "bench_main", "--no-run"], path, env,
            )

        samples: dict[str, dict[str, list[float]]] = {
            name: {str(count): [] for count in COUNTS} for name in variants
        }
        for i in range(args.warmup + args.runs):
            order = ("base", "candidate") if i % 2 == 0 else ("candidate", "base")
            for name in order:
                print(f"[{'warmup' if i < args.warmup else 'sample'} {i + 1}] {name}", flush=True)
                execute(
                    ["cargo", "bench", "--locked", "--no-default-features",
                     "--bench", "bench_main", "--", BENCHMARK_GROUP,
                     "--sample-size", "10", "--warm-up-time", "0.2",
                     "--measurement-time", "1", "--noplot"], variants[name], envs[name], cpu,
                )
                recorded = read_estimates(Path(envs[name]["CARGO_TARGET_DIR"]))
                if i >= args.warmup:
                    for count, estimate in recorded.items():
                        samples[name][count].append(estimate)

        comparisons = {
            str(count): summarize_pairs(
                samples["base"][str(count)], samples["candidate"][str(count)]
            ) for count in COUNTS
        }
        report = {
            "schema": 1,
            "methodology": "exact-base same-fixture paired Criterion, 1 pinned CPU",
            "base_ref": args.base_ref,
            "base_sha": execute(["git", "rev-parse", "HEAD"], base).strip(),
            "candidate_sha": execute(["git", "rev-parse", "HEAD"], ROOT).strip(),
            "fixture_sha256": hashlib.sha256(fixture.encode()).hexdigest(),
            "cpu": cpu,
            "warmup_rounds": args.warmup,
            "measured_rounds": args.runs,
            "samples_ns": samples,
            "comparison": comparisons,
        }
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        for count, row in comparisons.items():
            print(
                f"fabric_tail_read/{count}: "
                f"{row['median_base_ns']:.1f} ns base, "
                f"{row['median_candidate_ns']:.1f} ns candidate, "
                f"{row['median_speedup_x']:.3f}x paired speedup"
            )
        print(f"wrote {args.output}", flush=True)
        return 0
    finally:
        if created:
            subprocess.run(
                ["git", "worktree", "remove", "--force", str(base)],
                cwd=ROOT, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
