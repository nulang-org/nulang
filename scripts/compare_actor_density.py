#!/usr/bin/env python3
"""Compare actor-density probe outputs from a PR base and candidate.

The two binaries are run on the same CI host so ratios are substantially more
useful than comparing unrelated hosted-runner jobs. The gate is intentionally
coarse: memory must improve materially, while spawn/fan-out throughput is
allowed 25% noise/regression headroom before failing.
"""

from __future__ import annotations

import argparse
import math
from pathlib import Path


COUNTS = (10_000, 100_000)


def parse_metrics(path: Path) -> dict[str, float]:
    metrics: dict[str, float] = {}
    for raw in path.read_text().splitlines():
        if "=" not in raw:
            continue
        key, value = raw.split("=", 1)
        value = value.strip()
        if value == "unavailable":
            continue
        try:
            metrics[key.strip()] = float(value)
        except ValueError:
            continue
    return metrics


def ratio(candidate: float, base: float) -> float:
    if base == 0:
        return math.inf if candidate else 1.0
    return candidate / base


def pct_delta(candidate: float, base: float) -> float:
    return (ratio(candidate, base) - 1.0) * 100.0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-dir", type=Path, required=True)
    parser.add_argument("--candidate-dir", type=Path, required=True)
    args = parser.parse_args()

    failures: list[str] = []
    rows: list[str] = []

    for count in COUNTS:
        base = parse_metrics(args.base_dir / f"{count}.txt")
        cand = parse_metrics(args.candidate_dir / f"{count}.txt")

        required = (
            "initial_actor_heap_capacity_bytes_per_actor",
            "rss_spawn_delta_bytes_per_actor",
            "actors_per_second",
            "fanout_messages_per_second",
        )
        missing = [key for key in required if key not in base or key not in cand]
        if missing:
            failures.append(f"{count}: missing metrics: {', '.join(missing)}")
            continue

        heap_ratio = ratio(
            cand["initial_actor_heap_capacity_bytes_per_actor"],
            base["initial_actor_heap_capacity_bytes_per_actor"],
        )
        rss_ratio = ratio(
            cand["rss_spawn_delta_bytes_per_actor"],
            base["rss_spawn_delta_bytes_per_actor"],
        )
        spawn_ratio = ratio(cand["actors_per_second"], base["actors_per_second"])
        fanout_ratio = ratio(
            cand["fanout_messages_per_second"], base["fanout_messages_per_second"]
        )

        rows.append(
            "| {count:,} | {base_rss:.1f} → {cand_rss:.1f} ({rss:+.1f}%) | "
            "{base_heap:.1f} → {cand_heap:.1f} ({heap:+.1f}%) | "
            "{spawn:+.1f}% | {fanout:+.1f}% |".format(
                count=count,
                base_rss=base["rss_spawn_delta_bytes_per_actor"],
                cand_rss=cand["rss_spawn_delta_bytes_per_actor"],
                rss=pct_delta(
                    cand["rss_spawn_delta_bytes_per_actor"],
                    base["rss_spawn_delta_bytes_per_actor"],
                ),
                base_heap=base["initial_actor_heap_capacity_bytes_per_actor"],
                cand_heap=cand["initial_actor_heap_capacity_bytes_per_actor"],
                heap=pct_delta(
                    cand["initial_actor_heap_capacity_bytes_per_actor"],
                    base["initial_actor_heap_capacity_bytes_per_actor"],
                ),
                spawn=pct_delta(cand["actors_per_second"], base["actors_per_second"]),
                fanout=pct_delta(
                    cand["fanout_messages_per_second"],
                    base["fanout_messages_per_second"],
                ),
            )
        )

        # The candidate intentionally cuts the initial bump block from 16 KiB
        # to 2 KiB, so capacity should be <= 25% of the baseline even allowing
        # future representation changes.
        if heap_ratio > 0.25:
            failures.append(
                f"{count}: actor heap capacity ratio {heap_ratio:.3f} exceeds 0.25"
            )

        # RSS is noisier because allocator/runner state contributes to the
        # process, but a density optimization should still show >=25% savings.
        if rss_ratio > 0.75:
            failures.append(
                f"{count}: RSS/actor ratio {rss_ratio:.3f} did not improve by 25%"
            )

        # Same-host sequential runs still have scheduler/thermal variance.
        # Fail only on a material throughput regression.
        if spawn_ratio < 0.75:
            failures.append(
                f"{count}: spawn throughput ratio {spawn_ratio:.3f} is below 0.75"
            )
        if fanout_ratio < 0.75:
            failures.append(
                f"{count}: fan-out throughput ratio {fanout_ratio:.3f} is below 0.75"
            )

    print("## Actor density comparison")
    print()
    print("| Actors | RSS bytes/actor | Heap capacity/actor | Spawn Δ | Fan-out Δ |")
    print("|---:|---:|---:|---:|---:|")
    for row in rows:
        print(row)

    if failures:
        print()
        print("### Gate failures")
        for failure in failures:
            print(f"- {failure}")
        return 1

    print()
    print("Gate passed: memory improved materially without >25% spawn/fan-out regression.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
