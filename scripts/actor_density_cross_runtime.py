#!/usr/bin/env python3
"""Same-host, independent-process comparison of idle actor spawn and residency.

Nulang's existing actor_density example and Ractor's spawn_instant fixture
use different lifecycle APIs: their measurements are diagnostic rather than
perfectly equivalent. Do not turn these into a pass/fail language ranking.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import statistics
from pathlib import Path

try:
    from scripts import cross_runtime_bench as shared
except ModuleNotFoundError:  # direct: python3 scripts/actor_density_cross_runtime.py
    import cross_runtime_bench as shared

ROOT = Path(__file__).resolve().parents[1]
RUNTIMES = ("nulang", "ractor")
MAX_ACTORS = 1_000_000


def parse_actor_counts(text: str) -> list[int]:
    parts = [piece.strip() for piece in text.split(",")]
    if not parts or any(not part for part in parts):
        raise ValueError("actor counts must be a nonempty comma-separated list")
    try:
        values = [int(part) for part in parts]
    except ValueError as exc:
        raise ValueError("actor counts must be integers") from exc
    if any(not 1 <= value <= MAX_ACTORS for value in values):
        raise ValueError(f"actor counts must be between 1 and {MAX_ACTORS}")
    if len(set(values)) != len(values):
        raise ValueError("duplicate actor counts")
    return values


def parse_probe(output: str, runtime: str, actors: int) -> dict[str, object]:
    if runtime not in RUNTIMES:
        raise ValueError(f"unknown runtime: {runtime}")
    fields: dict[str, str] = {}
    for line in output.splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        if key in fields:
            raise RuntimeError(f"duplicate actor-density field: {key}")
        fields[key] = value.strip()

    if fields.get("mode") != "idle":
        raise RuntimeError(f"{runtime}: expected mode=idle")
    if fields.get("runtime", "nulang") != runtime:
        raise RuntimeError(f"{runtime}: incorrect runtime label")
    try:
        measured_count = int(fields["actor_count"])
    except (ValueError, KeyError) as exc:
        raise RuntimeError(f"{runtime}: invalid actor count") from exc
    if measured_count != actors:
        raise RuntimeError(f"{runtime}: actor count {measured_count} != {actors}")

    try:
        ns = float(fields["spawn_ns_per_item"])
        settle = float(fields["scheduler_settle_seconds"])
    except (KeyError, ValueError) as exc:
        raise RuntimeError(f"{runtime}: invalid spawn_ns_per_item or settle") from exc
    if not math.isfinite(ns) or ns <= 0:
        raise RuntimeError(f"{runtime}: spawn_ns_per_item must be finite and positive")
    if not math.isfinite(settle) or settle < 0:
        raise RuntimeError(f"{runtime}: scheduler settle must be finite and nonnegative")

    def rss(field: str) -> int | None:
        raw = fields.get(field)
        if raw is None:
            raise RuntimeError(f"{runtime}: missing {field}")
        if raw == "unavailable":
            return None
        try:
            number = int(raw)
        except ValueError as exc:
            raise RuntimeError(f"{runtime}: invalid {field}") from exc
        if number < 0:
            raise RuntimeError(f"{runtime}: negative {field}")
        return number

    before = rss("rss_before_bytes")
    after_spawn = rss("rss_after_spawn_bytes")
    after_settle = rss("rss_after_settle_bytes")

    def delta_per_actor(after: int | None) -> float | None:
        if before is None or after is None or after <= before:
            # Treat negative/zero RSS deltas as unavailable, NOT zero memory.
            return None
        return (after - before) / actors

    return {
        "runtime": runtime,
        "actors": actors,
        "spawn_ns_per_actor": ns,
        "scheduler_settle_seconds": settle,
        "spawn_rss_bytes_per_actor": delta_per_actor(after_spawn),
        "settled_rss_bytes_per_actor": delta_per_actor(after_settle),
    }


def summarize(rows: list[dict[str, object]]) -> dict[str, object]:
    if not rows:
        raise RuntimeError("no samples in actor-density summary")
    counts = {row["actors"] for row in rows}
    runtimes = {row["runtime"] for row in rows}
    if len(counts) != 1 or len(runtimes) != 1:
        raise RuntimeError("actor count or runtime changed between measurements")
    ns = [float(row["spawn_ns_per_actor"]) for row in rows]
    rss = [
        float(row["settled_rss_bytes_per_actor"])
        for row in rows
        if row["settled_rss_bytes_per_actor"] is not None
    ]
    median_ns = statistics.median(ns)
    return {
        "samples": len(rows),
        "actors": counts.pop(),
        "median_spawn_ns_per_actor": median_ns,
        "median_spawn_actors_per_second": 1_000_000_000 / median_ns,
        "min_spawn_ns_per_actor": min(ns),
        "max_spawn_ns_per_actor": max(ns),
        "rss_samples": len(rss),
        "median_settled_rss_bytes_per_actor": statistics.median(rss) if rss else None,
    }


def build_commands(selected: list[str]) -> dict[str, list[str]]:
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError("cargo is required to build actor-density fixtures")

    result: dict[str, list[str]] = {}
    if "nulang" in selected:
        shared.command_output([
            cargo, "build", "--locked", "--release", "--no-default-features",
            "--example", "actor_density",
        ])
        metadata = json.loads(
            shared.command_output([cargo, "metadata", "--format-version", "1", "--no-deps"])
        )
        target = Path(metadata["target_directory"])
        result["nulang"] = [
            str(target / "release" / "examples" / ("actor_density.exe" if os.name == "nt" else "actor_density"))
        ]

    if "ractor" in selected:
        manifest = shared.FIXTURES / "ractor_baseline" / "Cargo.toml"
        # Lockfile is generated in the independent workspace until pinned in git.
        shared.command_output([
            cargo, "build", "--release", "--manifest-path", str(manifest),
            "--bin", "density",
        ])
        metadata = json.loads(shared.command_output([
            cargo, "metadata", "--no-deps", "--format-version", "1",
            "--manifest-path", str(manifest),
        ]))
        target = Path(metadata["target_directory"])
        result["ractor"] = [
            str(target / "release" / ("density.exe" if os.name == "nt" else "density"))
        ]

    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--actors", default="1000,10000")
    parser.add_argument("--runtimes", default="nulang,ractor")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--cpu-mode", choices=("single", "host"), default="single")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    try:
        counts = parse_actor_counts(args.actors)
        selected = [name.strip() for name in args.runtimes.split(",")]
        if not selected or any(name not in RUNTIMES for name in selected):
            raise ValueError("runtimes must be a subset of nulang,ractor")
        if len(selected) != len(set(selected)):
            raise ValueError("duplicate runtime")
    except ValueError as exc:
        parser.error(str(exc))
    if args.runs <= 0 or args.warmup < 0:
        parser.error("--runs must be positive and --warmup must be nonnegative")

    affinity = shared.measurement_affinity(args.cpu_mode)
    commands = build_commands(selected)
    samples: dict[str, dict[str, list[dict[str, object]]]] = {
        runtime: {str(count): [] for count in counts} for runtime in selected
    }
    for iteration in range(args.warmup + args.runs):
        measured = iteration >= args.warmup
        order = [(runtime, count) for count in counts for runtime in selected]
        if iteration % 2:
            order.reverse()
        for runtime, actors in order:
            print(f"[{'sample' if measured else 'warmup'}] {runtime}/{actors}", flush=True)
            output = shared.command_output(
                [*commands[runtime], "idle", str(actors)],
                cpu_affinity=affinity,
            )
            row = parse_probe(output, runtime, actors)
            if measured:
                samples[runtime][str(actors)].append(row)

    summary = {
        runtime: {str(count): summarize(samples[runtime][str(count)]) for count in counts}
        for runtime in selected
    }
    print("runtime    actors   median actors/s   settled RSS bytes/actor")
    for runtime in selected:
        for count in counts:
            row = summary[runtime][str(count)]
            rss = row["median_settled_rss_bytes_per_actor"]
            print(
                f"{runtime:<9}  {count:>7,}   {row['median_spawn_actors_per_second']:>15,.0f}   "
                f"{rss:,.1f}" if rss is not None else
                f"{runtime:<9}  {count:>7,}   {row['median_spawn_actors_per_second']:>15,.0f}   unavailable"
            )
    report = {
        "schema": 1,
        "methodology": "same-host non-durable idle actor density; spawn lifecycle differs",
        "measured_runs": args.runs,
        "warmup_runs": args.warmup,
        "environment": shared.environment_metadata(args.cpu_mode, affinity),
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
