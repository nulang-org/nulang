#!/usr/bin/env python3
"""Diagnostic comparison of raw Nulang mailbox admission and Ractor ActorRef.cast.

Important: the operations are not semantically equivalent (raw mailbox vs
actor-reference routing) and must NOT be presented as actor-runtime speedups.
"""
from __future__ import annotations

import argparse
import json
import re
import shutil
import statistics
from pathlib import Path

try:
    from scripts import cross_runtime_bench as shared
except ModuleNotFoundError:
    import cross_runtime_bench as shared

RUNTIMES = ("nulang", "ractor")
RECORD = re.compile(
    r"\[contention-bench\] runtime=(?P<runtime>[a-z_-]+) "
    r"producers=(?P<producers>\d+) messages=(?P<messages>\d+) "
    r"elapsed_ns=(?P<elapsed_ns>\d+)"
)


def parse_producers(raw: str) -> list[int]:
    tokens = [token.strip() for token in raw.split(",")]
    if not tokens or any(not token for token in tokens):
        raise ValueError("empty producer count")
    try:
        values = [int(token) for token in tokens]
    except ValueError as exc:
        raise ValueError("producer counts must be integers") from exc
    if any(not 1 <= value <= 64 for value in values):
        raise ValueError("producers must be between 1 and 64")
    if len(set(values)) != len(values):
        raise ValueError("duplicate producer counts")
    return values


def parse_record(
    output: str, runtime: str, producers: int, expected_messages: int
) -> dict[str, int]:
    matches = [match for match in RECORD.finditer(output)
               if match.group("runtime") == runtime]
    if not matches:
        raise RuntimeError(f"{runtime}: missing contention benchmark record")
    if len(matches) != 1:
        raise RuntimeError(f"{runtime}: duplicate contention benchmark records")
    match = matches[0]
    actual_producers = int(match.group("producers"))
    messages = int(match.group("messages"))
    elapsed = int(match.group("elapsed_ns"))
    if actual_producers != producers:
        raise RuntimeError(f"{runtime}: producers {actual_producers} != {producers}")
    if messages != expected_messages:
        raise RuntimeError(f"{runtime}: messages {messages} != {expected_messages}")
    if elapsed <= 0:
        raise RuntimeError(f"{runtime}: elapsed_ns must be positive")
    return {"producers": producers, "messages": messages, "elapsed_ns": elapsed}


def summarize(rows: list[dict[str, int]]) -> dict[str, float | int]:
    if not rows:
        raise RuntimeError("no samples for mailbox contention benchmark")
    counts = {row["messages"] for row in rows}
    producers = {row["producers"] for row in rows}
    if len(counts) != 1 or len(producers) != 1:
        raise RuntimeError("message counts or producers changed between samples")
    durations = [row["elapsed_ns"] for row in rows]
    if any(value <= 0 for value in durations):
        raise RuntimeError("sample elapsed time must be positive")
    median = statistics.median(durations)
    messages = counts.pop()
    return {
        "samples": len(rows),
        "messages": messages,
        "producers": producers.pop(),
        "median_elapsed_ns": median,
        "median_messages_per_second": messages * 1_000_000_000 / median,
        "min_elapsed_ns": min(durations),
        "max_elapsed_ns": max(durations),
    }


def build_commands() -> dict[str, list[str]]:
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError("Rust cargo is required for both benchmark fixtures")
    shared.command_output([
        cargo, "build", "--locked", "--release", "--no-default-features",
        "--example", "mailbox_contention",
    ])
    main_metadata = json.loads(
        shared.command_output([cargo, "metadata", "--no-deps", "--format-version", "1"])
    )
    cargo_manifest = shared.FIXTURES / "ractor_baseline" / "Cargo.toml"
    shared.command_output([
        cargo, "build", "--locked", "--release", "--manifest-path",
        str(cargo_manifest), "--bin", "contention",
    ])
    ractor_metadata = json.loads(shared.command_output([
        cargo, "metadata", "--no-deps", "--format-version", "1",
        "--manifest-path", str(cargo_manifest),
    ]))
    return {
        "nulang": [
            str(Path(main_metadata["target_directory"]) / "release" / "examples" / "mailbox_contention")
        ],
        "ractor": [
            str(Path(ractor_metadata["target_directory"]) / "release" / "contention")
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--producers", default="1,2,4,8")
    parser.add_argument("--messages-per-producer", type=int, default=10000)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--cpu-mode", choices=("host", "single"), default="host")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    try:
        producers = parse_producers(args.producers)
    except ValueError as exc:
        parser.error(str(exc))
    if not 1 <= args.messages_per_producer <= 100_000:
        parser.error("--messages-per-producer must be 1..100000")
    if args.runs < 1 or args.warmup < 0:
        parser.error("--runs must be positive and --warmup nonnegative")

    commands = build_commands()
    affinity = shared.measurement_affinity(args.cpu_mode)
    samples: dict[str, dict[str, list[dict[str, int]]]] = {
        runtime: {str(count): [] for count in producers} for runtime in RUNTIMES
    }
    for iteration in range(args.runs + args.warmup):
        measured = iteration >= args.warmup
        order = [(runtime, count) for count in producers for runtime in RUNTIMES]
        if iteration % 2:
            order.reverse()
        for runtime, count in order:
            print(
                f"[{'sample' if measured else 'warmup'}] {runtime} producers={count}",
                flush=True
            )
            stdout = shared.command_output(
                [*commands[runtime], str(count), str(args.messages_per_producer)],
                cpu_affinity=affinity,
            )
            row = parse_record(
                stdout, runtime, count, count * args.messages_per_producer
            )
            if measured:
                samples[runtime][str(count)].append(row)

    summary = {
        runtime: {str(n): summarize(samples[runtime][str(n)]) for n in producers}
        for runtime in RUNTIMES
    }
    print("runtime   producers   median admitted messages/sec (not equivalent APIs)")
    for runtime in RUNTIMES:
        for count in producers:
            speed = summary[runtime][str(count)]["median_messages_per_second"]
            print(f"{runtime:<9} {count:>9}   {speed:>17,.0f}")
    report = {
        "schema": 1,
        "comparison_class": {
            "nulang": "low-level lock-free Mailbox::push only",
            "ractor": "Ractor ActorRef::cast actor-mailbox admission",
        },
        "methodology": "diagnostic concurrent producers, consumer paused during enqueue",
        "environment": shared.environment_metadata(args.cpu_mode, affinity),
        "messages_per_producer": args.messages_per_producer,
        "measured_runs": args.runs,
        "warmup_runs": args.warmup,
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
