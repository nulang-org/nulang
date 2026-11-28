#!/usr/bin/env python3
"""Paired same-binary comparison of per-connection and cross-client WAL barriers."""
import argparse
import json
import os
from pathlib import Path
import platform
import statistics

from fabric_resp_ab_bench import binary_hash, server, summarize
from fabric_resp_concurrent_bench import measure_concurrent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fabric", required=True, type=Path)
    parser.add_argument("--journal-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--commands", type=int, default=64)
    args = parser.parse_args()
    if args.rounds <= 0 or args.commands <= 0 or args.commands % 32:
        parser.error("positive rounds and commands divisible by 32 required")
    binary = args.fabric.resolve()
    report = {"platform": platform.platform(), "load_average": os.getloadavg(),
              "binary_sha256": binary_hash(binary), "rounds": args.rounds,
              "commands_per_client": args.commands, "clock_reservation_ms": 1000,
              "expected_cases": 4, "cases": []}
    for clients in (8, 32):
        for depth in (1, 32):
            samples = {"per_connection": [], "cross_client": []}
            for index in range(args.rounds):
                order = ("per_connection", "cross_client") if index % 2 == 0 else ("cross_client", "per_connection")
                for mode in order:
                    with server(binary, "fabric", "sync", args.journal_dir, 1000, True, mode == "cross_client") as sock:
                        samples[mode].append(measure_concurrent(sock.getpeername(), "set", depth, args.commands, clients))
            medians = {mode: summarize([{k: v for k, v in s.items() if k != "client_completion_ms"} for s in rounds]) for mode, rounds in samples.items()}
            paired = [b["ops_per_second"] / a["ops_per_second"] for a, b in zip(samples["per_connection"], samples["cross_client"])]
            case = {"clients": clients, "pipeline": depth, "samples": samples,
                    "median": medians, "paired_speedups": paired,
                    "median_paired_speedup": statistics.median(paired)}
            report["cases"].append(case)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
            print(clients, depth, {m: round(s["ops_per_second"]) for m, s in medians.items()}, "paired speedup", round(case["median_paired_speedup"], 2), flush=True)


if __name__ == "__main__":
    main()
