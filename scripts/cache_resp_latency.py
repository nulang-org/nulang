#!/usr/bin/env python3
"""End-to-end RESP tail-latency gate for the Nulang cache.

Measures the real TCP/Mio server and compares it with Valkey on the same CI
runner. The gate intentionally uses generous absolute floors plus same-runner
relative limits so shared-runner noise does not turn small latency differences
into flaky failures.
"""

from __future__ import annotations

import argparse
import json
import math
import time
from pathlib import Path

from cache_valkey_diff import RespClient, parse_endpoint


def percentile_ns(samples: list[int], quantile: float) -> int:
    if not samples:
        raise ValueError("percentile requires at least one sample")
    ordered = sorted(samples)
    rank = max(1, math.ceil(quantile * len(ordered)))
    return ordered[min(rank - 1, len(ordered) - 1)]


def summarize(samples: list[int]) -> dict[str, float | int]:
    return {
        "samples": len(samples),
        "p50_us": percentile_ns(samples, 0.50) / 1_000.0,
        "p95_us": percentile_ns(samples, 0.95) / 1_000.0,
        "p99_us": percentile_ns(samples, 0.99) / 1_000.0,
        "p999_us": percentile_ns(samples, 0.999) / 1_000.0,
        "max_us": max(samples) / 1_000.0,
    }


def command_samples(
    client: RespClient,
    command: tuple[bytes, ...],
    *,
    warmup: int,
    samples: int,
) -> list[int]:
    for _ in range(warmup):
        client.command(*command)

    timings: list[int] = []
    for _ in range(samples):
        start = time.perf_counter_ns()
        client.command(*command)
        timings.append(time.perf_counter_ns() - start)
    return timings


def pipeline_samples(
    client: RespClient,
    command: tuple[bytes, ...],
    *,
    pipeline_depth: int,
    warmup_batches: int,
    batches: int,
) -> tuple[list[int], list[int]]:
    batch = [command] * pipeline_depth
    for _ in range(warmup_batches):
        client.pipeline(batch)

    batch_timings: list[int] = []
    normalized_timings: list[int] = []
    for _ in range(batches):
        start = time.perf_counter_ns()
        client.pipeline(batch)
        elapsed = time.perf_counter_ns() - start
        batch_timings.append(elapsed)
        normalized_timings.append(elapsed // pipeline_depth)

    return batch_timings, normalized_timings


def threshold(valkey_us: float, *, ratio: float, absolute_floor_us: float) -> float:
    return max(valkey_us * ratio, absolute_floor_us)


def gate_metric(
    failures: list[str],
    label: str,
    nulang_us: float,
    valkey_us: float,
    *,
    ratio: float,
    absolute_floor_us: float,
) -> None:
    allowed = threshold(
        valkey_us,
        ratio=ratio,
        absolute_floor_us=absolute_floor_us,
    )
    if nulang_us > allowed:
        failures.append(
            f"{label}: Nulang {nulang_us:.1f}us exceeds "
            f"allowed {allowed:.1f}us "
            f"(Valkey {valkey_us:.1f}us, ratio {ratio:.1f}x)"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--nulang", type=parse_endpoint, default=("127.0.0.1", 6380))
    parser.add_argument("--valkey", type=parse_endpoint, default=("127.0.0.1", 6379))
    parser.add_argument("--samples", type=int, default=20_000)
    parser.add_argument("--warmup", type=int, default=2_000)
    parser.add_argument("--pipeline-depth", type=int, default=32)
    parser.add_argument("--pipeline-batches", type=int, default=2_000)
    parser.add_argument("--pipeline-warmup-batches", type=int, default=100)
    parser.add_argument("--output", type=Path, default=Path("cache-resp-latency.json"))
    args = parser.parse_args()

    if args.samples < 1_000:
        parser.error("--samples must be at least 1000 for meaningful p99.9")
    if args.pipeline_depth <= 0 or args.pipeline_batches <= 0:
        parser.error("pipeline depth and batch count must be positive")

    nulang = RespClient.connect(*args.nulang)
    valkey = RespClient.connect(*args.valkey)
    try:
        key = b"latency:{same-slot}:key"
        value = b"x" * 64
        for client in (nulang, valkey):
            reply = client.command(b"SET", key, value)
            if reply != ("simple", b"OK"):
                raise RuntimeError(f"SET fixture failed: {reply!r}")

        get_command = (b"GET", key)
        nulang_seq = command_samples(
            nulang,
            get_command,
            warmup=args.warmup,
            samples=args.samples,
        )
        valkey_seq = command_samples(
            valkey,
            get_command,
            warmup=args.warmup,
            samples=args.samples,
        )

        nulang_batch, nulang_pipe = pipeline_samples(
            nulang,
            get_command,
            pipeline_depth=args.pipeline_depth,
            warmup_batches=args.pipeline_warmup_batches,
            batches=args.pipeline_batches,
        )
        valkey_batch, valkey_pipe = pipeline_samples(
            valkey,
            get_command,
            pipeline_depth=args.pipeline_depth,
            warmup_batches=args.pipeline_warmup_batches,
            batches=args.pipeline_batches,
        )

        report = {
            "schema_version": 1,
            "operation": "GET hit, 64-byte value",
            "sequential": {
                "nulang": summarize(nulang_seq),
                "valkey": summarize(valkey_seq),
            },
            "pipeline": {
                "depth": args.pipeline_depth,
                "batch_latency": {
                    "nulang": summarize(nulang_batch),
                    "valkey": summarize(valkey_batch),
                },
                "normalized_per_command": {
                    "nulang": summarize(nulang_pipe),
                    "valkey": summarize(valkey_pipe),
                },
            },
            "gate_policy": {
                "sequential_p99": {"ratio": 4.0, "absolute_floor_us": 2_000.0},
                "sequential_p999": {"ratio": 6.0, "absolute_floor_us": 5_000.0},
                "pipeline_p99": {"ratio": 4.0, "absolute_floor_us": 500.0},
                "pipeline_p999": {"ratio": 6.0, "absolute_floor_us": 1_500.0},
            },
        }

        failures: list[str] = []
        seq_n = report["sequential"]["nulang"]
        seq_v = report["sequential"]["valkey"]
        pipe_n = report["pipeline"]["normalized_per_command"]["nulang"]
        pipe_v = report["pipeline"]["normalized_per_command"]["valkey"]

        gate_metric(
            failures,
            "sequential p99",
            float(seq_n["p99_us"]),
            float(seq_v["p99_us"]),
            ratio=4.0,
            absolute_floor_us=2_000.0,
        )
        gate_metric(
            failures,
            "sequential p99.9",
            float(seq_n["p999_us"]),
            float(seq_v["p999_us"]),
            ratio=6.0,
            absolute_floor_us=5_000.0,
        )
        gate_metric(
            failures,
            "pipeline normalized p99",
            float(pipe_n["p99_us"]),
            float(pipe_v["p99_us"]),
            ratio=4.0,
            absolute_floor_us=500.0,
        )
        gate_metric(
            failures,
            "pipeline normalized p99.9",
            float(pipe_n["p999_us"]),
            float(pipe_v["p999_us"]),
            ratio=6.0,
            absolute_floor_us=1_500.0,
        )

        report["failures"] = failures
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(json.dumps(report, indent=2, sort_keys=True))

        if failures:
            print("\nRESP tail-latency gate failed:")
            for failure in failures:
                print(f"  - {failure}")
            return 1

        print("\nRESP tail-latency gate passed")
        return 0
    finally:
        nulang.close()
        valkey.close()


if __name__ == "__main__":
    raise SystemExit(main())
