#!/usr/bin/env python3
"""Compare single-node memory GET workloads across reproducible key distributions.

Closed-loop batch RTT includes Python overhead and omits open-loop queuing.
No production capacity or performance-gate claims.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import platform
import random
import socket
import subprocess
import threading
import time

from fabric_resp_ab_bench import OK, binary_hash, exchange, percentile, resp, server, summarize

from fabric_process_metrics import read_snapshot, summarize_process

DISTRIBUTIONS = ("shared", "uniform", "hot90")
MAX_KEYS = 65536
MAX_COMMANDS = 65536


def key(index: int) -> bytes:
    return f"bench:key:{index:08d}".encode()


def value(index: int) -> bytes:
    return f"{index:064d}".encode()


def trace(distribution: str, keys: int, commands: int, seed: int) -> list[int]:
    if distribution not in DISTRIBUTIONS or not 1 <= keys <= MAX_KEYS or not 1 <= commands <= MAX_COMMANDS:
        raise ValueError("invalid distribution, keyspace or command count")
    if distribution == "hot90" and keys < 2:
        raise ValueError("hot90 requires at least two keys")
    rng = random.Random(seed)
    if distribution == "shared":
        return [0] * commands
    if distribution == "uniform":
        return [rng.randrange(keys) for _ in range(commands)]
    return [0 if rng.randrange(10) < 9 else rng.randrange(1, keys) for _ in range(commands)]


def batch(indices: list[int]) -> tuple[bytes, bytes]:
    return (b"".join(resp(b"GET", key(i)) for i in indices),
            b"".join(b"$64\r\n" + value(i) + b"\r\n" for i in indices))


def measure(control: socket.socket, distribution: str, keys: int, commands: int,
            pipeline: int, clients: int, seed: int, pid: int | None = None) -> dict:
    if not 1 <= clients <= 63 or not 1 <= pipeline <= 128 or commands % pipeline:
        raise ValueError("invalid client count, pipeline or command divisibility")
    # All workers share the same prepopulated dataset; setup is untimed.
    traces = [trace(distribution, keys, commands, seed + i) for i in range(clients)]
    idle = read_snapshot(pid) if pid is not None else None
    for offset in range(0, keys, 32):
        count = min(32, keys - offset)
        exchange(control, b"".join(resp(b"SET", key(i), value(i))
                                   for i in range(offset, offset + count)), OK * count)
    seeded = read_snapshot(pid) if pid is not None else None
    address = control.getpeername()
    start_ns = 0
    start_snapshot = None

    def start():
        nonlocal start_ns, start_snapshot
        if pid is not None:
            start_snapshot = read_snapshot(pid)
        start_ns = time.perf_counter_ns()

    barrier = threading.Barrier(clients, action=start, timeout=30)

    def worker(indices):
        try:
            with socket.create_connection(address, timeout=10) as sock:
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                for offset in range(0, min(commands, 8 * pipeline), pipeline):
                    exchange(sock, *batch(indices[offset:offset + pipeline]))
                barrier.wait()
                samples = []
                for offset in range(0, commands, pipeline):
                    wire, expected = batch(indices[offset:offset + pipeline])
                    begin = time.perf_counter_ns()
                    exchange(sock, wire, expected)
                    samples.append((time.perf_counter_ns() - begin) / 1000)
                return samples, time.perf_counter_ns()
        except BaseException:
            barrier.abort()
            raise

    with ThreadPoolExecutor(max_workers=clients) as executor:
        futures = [executor.submit(worker, indices) for indices in traces]
        results, errors = [], []
        for future in futures:
            try:
                results.append(future.result())
            except Exception as error:
                errors.append(error)
        if errors:
            raise next((e for e in errors if not isinstance(e, threading.BrokenBarrierError)), errors[0])
    end_snapshot = read_snapshot(pid) if pid is not None else None
    latencies = [sample for samples, _ in results for sample in samples]
    total = clients * commands
    if len(latencies) != total // pipeline:
        raise RuntimeError("incomplete benchmark sample")
    elapsed = (max(end for _, end in results) - start_ns) / 1e9
    result = {
        "commands": total, "batches": len(latencies),
        "ops_per_second": total / elapsed,
        "batch_p50_us": percentile(latencies, .50),
        "batch_p95_us": percentile(latencies, .95),
        "batch_p99_us": percentile(latencies, .99),
        "batch_p999_us": percentile(latencies, .999),
        "key_zero_requests": sum(indices.count(0) for indices in traces),
        "distinct_requested_keys": len({i for indices in traces for i in indices}),
    }
    if pid is not None:
        result.update(summarize_process(idle, seeded, start_snapshot, end_snapshot, keys, total))
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--fabric", required=True, type=Path)
    p.add_argument("--valkey", required=True, type=Path)
    p.add_argument("--journal-dir", required=True, type=Path)
    p.add_argument("--output", required=True, type=Path)
    p.add_argument("--distribution", action="append", choices=DISTRIBUTIONS)
    p.add_argument("--keys", type=int, default=4096)
    p.add_argument("--commands", type=int, default=8192)
    p.add_argument("--pipeline", type=int, default=32)
    p.add_argument("--clients", type=int, default=8)
    p.add_argument("--seed", type=int, default=42)
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--process-metrics", action="store_true", help="collect Linux server CPU time and RSS")
    args = p.parse_args()
    if args.process_metrics and platform.system() != "Linux":
        p.error("--process-metrics requires Linux /proc")
    distributions = args.distribution or list(DISTRIBUTIONS)
    if (not 1 <= args.keys <= MAX_KEYS or not 1 <= args.commands <= MAX_COMMANDS
            or not 1 <= args.pipeline <= 128 or args.commands % args.pipeline
            or not 1 <= args.clients <= 63 or args.rounds <= 0
            or len(set(distributions)) != len(distributions)
            or ("hot90" in distributions and args.keys < 2)):
        p.error("invalid or duplicate benchmark parameters")
    binaries = {"fabric": args.fabric.resolve(), "valkey": args.valkey.resolve()}
    report = {
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "load_average": os.getloadavg(), "rounds": args.rounds,
        "binary_sha256": {e: binary_hash(b) for e, b in binaries.items()},
        "valkey_version": subprocess.check_output([str(binaries["valkey"]), "--version"], text=True).strip(),
        "python_version": platform.python_version(),
        "mode": "memory", "workload": "get", "value_bytes": 64,
        "keys": args.keys, "clients": args.clients, "commands_per_client": args.commands,
        "pipeline": args.pipeline, "seed": args.seed,
        "latency_unit": "closed-loop batch round trip microseconds, pooled per round",
        "process_metrics": args.process_metrics,
        "cpu_tick_hz": os.sysconf("SC_CLK_TCK") if args.process_metrics else None,
        "expected_cases": len(distributions), "cases": [],
    }
    for distribution in distributions:
        samples = {e: [] for e in binaries}
        for r in range(args.rounds):
            order = ("fabric", "valkey") if r % 2 == 0 else ("valkey", "fabric")
            for engine in order:
                pids = []
                with server(binaries[engine], engine, "memory", args.journal_dir,
                            on_ready=pids.append if args.process_metrics else None) as control:
                    samples[engine].append(measure(control, distribution, args.keys,
                                                   args.commands, args.pipeline, args.clients, args.seed + r,
                                                   pid=pids[0] if args.process_metrics else None))
        medians = {e: summarize(s) for e, s in samples.items()}
        report["cases"].append({"distribution": distribution, "samples": samples, "median": medians})
        print(distribution, {e: round(s["ops_per_second"]) for e, s in medians.items()}, flush=True)
    # Write the report only after every benchmark case succeeds.
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
