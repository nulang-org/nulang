#!/usr/bin/env python3
"""Closed-loop concurrent RESP comparison with validated replies.

Uses independent client keys, synchronized starts, fixed work per client,
and pooled per-round batch latency. The Python load generator can itself
become a bottleneck. No performance gate or production capacity claim.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import threading
import time

from fabric_resp_ab_bench import (
    VALUE, HIT, OK, binary_hash, exchange, percentile, resp, server, summarize,
)


def measure_concurrent(address: tuple, name: str, pipeline: int,
                       commands: int, clients: int) -> dict:
    # One control connection is retained by server(); default peer cap is 64.
    if not 1 <= clients <= 63 or pipeline <= 0 or commands <= 0 or commands % pipeline:
        raise ValueError("require 1..63 clients and positive commands divisible by pipeline")
    if name not in ("get", "set", "ttl-get", "ttl-set"):
        raise ValueError("unknown workload")
    start_ns = 0

    def start():
        nonlocal start_ns
        start_ns = time.perf_counter_ns()

    barrier = threading.Barrier(clients, action=start, timeout=30)

    def worker(index: int):
        try:
            with socket.create_connection(address, timeout=10) as sock:
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                key = f"bench:{{client-{index}}}".encode()
                seed = resp(b"SET", key, VALUE, b"PX", b"3600000") if name.startswith("ttl-") else resp(b"SET", key, VALUE)
                exchange(sock, seed, OK)
                if name.endswith("get"):
                    wire, reply = resp(b"GET", key), HIT
                else:
                    wire, reply = seed, OK
                wire, reply = wire * pipeline, reply * pipeline
                for _ in range(min(8, commands // pipeline)):
                    exchange(sock, wire, reply)
                barrier.wait()
                samples = []
                for _ in range(commands // pipeline):
                    begin = time.perf_counter_ns()
                    exchange(sock, wire, reply)
                    samples.append((time.perf_counter_ns() - begin) / 1000)
                return samples, time.perf_counter_ns()
        except BaseException:
            barrier.abort()
            raise

    with ThreadPoolExecutor(max_workers=clients) as executor:
        futures = [executor.submit(worker, i) for i in range(clients)]
        # Prefer the original protocol/transport error to the barrier error
        # caused by it. No partial result is ever accepted.
        errors = []
        results = []
        for future in futures:
            try:
                results.append(future.result())
            except Exception as error:
                errors.append(error)
        if errors:
            raise next((e for e in errors if not isinstance(e, threading.BrokenBarrierError)), errors[0])
    latencies = [sample for samples, _ in results for sample in samples]
    completion = [(end - start_ns) / 1e6 for _, end in results]
    elapsed = max(completion) / 1000
    total = commands * clients
    assert len(latencies) == total // pipeline
    return {
        "clients": clients, "commands": total, "batches": len(latencies),
        "ops_per_second": total / elapsed,
        "batch_p50_us": percentile(latencies, .50),
        "batch_p95_us": percentile(latencies, .95),
        "batch_p99_us": percentile(latencies, .99),
        "batch_p999_us": percentile(latencies, .999),
        "client_completion_ms": completion,
        "slowest_client_ms": max(completion),
        "fastest_client_ms": min(completion),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fabric", required=True, type=Path)
    parser.add_argument("--valkey", required=True, type=Path)
    parser.add_argument("--journal-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--clients", action="append", type=int)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--memory-commands", type=int, default=8192)
    parser.add_argument("--sync-commands", type=int, default=128)
    args = parser.parse_args()
    clients = args.clients or [1, 8, 32]
    if len(set(clients)) != len(clients) or any(not 1 <= c <= 63 for c in clients):
        parser.error("clients must be distinct integers in 1..63")
    if args.rounds <= 0 or any(n <= 0 or n % 32 for n in (args.memory_commands, args.sync_commands)):
        parser.error("rounds positive; command counts positive multiples of 32")
    binaries = {"fabric": args.fabric.resolve(), "valkey": args.valkey.resolve()}
    report = {
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "load_average": os.getloadavg(), "rounds": args.rounds,
        "binary_sha256": {engine: binary_hash(path) for engine, path in binaries.items()},
        "valkey_version": subprocess.check_output([str(binaries["valkey"]), "--version"], text=True).strip(),
        "value_bytes": len(VALUE), "key_distribution": "one independent hot key per client",
        "latency_unit": "closed-loop batch round trip microseconds, pooled per round",
        "fabric_clock_reservation_ms": 1000, "fabric_group_commit": True,
        "expected_cases": 3 * len(clients), "cases": [],
    }
    # Memory read scaling; synchronous writes with and without pipelining.
    for mode, name, depth, commands in [
        ("memory", "get", 32, args.memory_commands),
        ("sync", "set", 1, args.sync_commands),
        ("sync", "set", 32, args.sync_commands),
    ]:
        for count in clients:
            samples = {engine: [] for engine in binaries}
            for round_index in range(args.rounds):
                order = ("fabric", "valkey") if round_index % 2 == 0 else ("valkey", "fabric")
                for engine in order:
                    with server(binaries[engine], engine, mode, args.journal_dir, 1000, True) as control:
                        samples[engine].append(measure_concurrent(control.getpeername(), name, depth, commands, count))
            # Retain every client's completion time in raw round summaries;
            # median aggregates are scalar only.
            medians = {engine: summarize([
                {k: v for k, v in sample.items() if k != "client_completion_ms"}
                for sample in rounds
            ]) for engine, rounds in samples.items()}
            case = {"mode": mode, "workload": name, "pipeline": depth,
                    "clients": count, "commands_per_client": commands,
                    "samples": samples, "median": medians}
            report["cases"].append(case)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
            print(mode, name, depth, count, {e: round(v["ops_per_second"]) for e, v in medians.items()}, flush=True)


if __name__ == "__main__":
    main()
