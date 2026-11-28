#!/usr/bin/env python3
"""Bounded, reply-checked concurrent Fabric soak and crash recovery."""
from __future__ import annotations

import argparse
from bisect import bisect_left
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import http.client
import json
import math
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import tempfile
import threading
import time

from fabric_resp_ab_bench import binary_hash, exchange, free_port, resp

TTL_SLOTS = 128
TTL_MS = 250
ENTRIES = 4096
ARENA_BYTES = 16 * 1024 * 1024
CONNECTION_BYTES = 8 * 1024 * 1024
JOURNAL_BYTES = 2 * 1024 * 1024
BOUNDS = (50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000, 50000,
          100000, 200000, 500000, 1000000, 2000000, 5000000, 10000000)
METRICS = (
    "healthy", "ready", "snapshot_fresh", "durable", "connections_active",
    "connection_budget_used_bytes", "connection_budget_limit_bytes",
    "cache_entries", "cache_arena_reserved_bytes", "cache_retained_capacity_bytes",
    "cache_expirations_total", "journal_bytes", "journal_limit_bytes",
    "checkpoint_completed_total", "checkpoint_failed_total",
    "checkpoint_skipped_total", "checkpoint_reclaimed_bytes_total",
    "wal_groups_synced_total", "wal_group_records_synced_total",
    "commands_dispatched_total", "connections_rejected_limit_total",
    "connections_rejected_peer_total", "accept_resource_errors_total",
)


class Histogram:
    def __init__(self):
        self.counts = [0] * (len(BOUNDS) + 1)
        self.maximum = 0

    def add(self, micros):
        self.counts[bisect_left(BOUNDS, micros)] += 1
        self.maximum = max(self.maximum, micros)

    def merge(self, other):
        self.counts = [a + b for a, b in zip(self.counts, other.counts)]
        self.maximum = max(self.maximum, other.maximum)

    def quantile(self, q):
        n = math.ceil(sum(self.counts) * q)
        if not n:
            return None
        for bound, count in zip((*BOUNDS, None), self.counts):
            n -= count
            if n <= 0:
                return bound
        raise AssertionError("invalid histogram")

    def report(self):
        return {"batches": sum(self.counts), "upper_bounds_us": list(BOUNDS),
                "counts": self.counts, "max_us": self.maximum,
                "p50_upper_us": self.quantile(.5), "p95_upper_us": self.quantile(.95),
                "p99_upper_us": self.quantile(.99), "p999_upper_us": self.quantile(.999)}


def value(seq):
    return str(seq).encode().rjust(256, b"x")


def counter_key(client):
    return f"counter:{client}".encode()


def stable_key(client):
    return f"stable:{client}".encode()


def ttl_key(client, seq):
    return f"ttl:{client}:{seq % TTL_SLOTS}".encode()


def bulk(data):
    return b"$%d\r\n" % len(data) + data + b"\r\n"


def batch(client, seq):
    wire, expected = bytearray(), bytearray()
    for n in range(seq + 1, seq + 5):
        v = value(n)
        wire.extend(resp(b"INCR", counter_key(client)))
        wire.extend(resp(b"SET", stable_key(client), v))
        wire.extend(resp(b"GET", stable_key(client)))
        wire.extend(resp(b"SET", ttl_key(client, n), v, b"PX", str(TTL_MS).encode()))
        expected.extend(b":%d\r\n" % n + b"+OK\r\n" + bulk(v) + b"+OK\r\n")
    return bytes(wire), bytes(expected), seq + 4


def parse_metrics(body):
    result = {}
    for line in body.splitlines():
        if not line or line.startswith("#"):
            continue
        key, v = line.split()
        if not key.startswith("fabric_"):
            continue
        key = key.removeprefix("fabric_")
        if key in result or not v.isascii() or not v.isdecimal():
            raise ValueError("duplicate or invalid metric")
        result[key] = int(v)
    if not set(METRICS) <= result.keys():
        raise ValueError("missing required metrics")
    return {key: result[key] for key in METRICS}


def validate_sample(sample):
    if any(sample[k] != 1 for k in ("healthy", "ready", "snapshot_fresh", "durable")):
        raise RuntimeError("unhealthy or stale metrics snapshot")
    for used, limit, configured in (
        ("connection_budget_used_bytes", "connection_budget_limit_bytes", CONNECTION_BYTES),
        ("journal_bytes", "journal_limit_bytes", JOURNAL_BYTES),
    ):
        if sample[limit] != configured or sample[used] > configured:
            raise RuntimeError(f"budget violated: {used}")
    if sample["cache_entries"] > ENTRIES or sample["cache_arena_reserved_bytes"] > ARENA_BYTES:
        raise RuntimeError("cache admission limit violated")
    for key in ("checkpoint_failed_total", "connections_rejected_limit_total",
                "connections_rejected_peer_total", "accept_resource_errors_total"):
        if sample[key]:
            raise RuntimeError(f"unexpected failure: {key}={sample[key]}")


def scrape(port):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        conn.request("GET", "/metrics")
        response = conn.getresponse()
        body = response.read(16385)
        if response.status != 200 or len(body) > 16384:
            raise RuntimeError("invalid metrics response")
        return parse_metrics(body.decode("ascii"))
    finally:
        conn.close()


def rss_bytes(pid):
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) * 1024
    raise RuntimeError("RSS missing")


@contextmanager
def instance(binary, journal, log_path):
    port, metrics = free_port(), free_port()
    while metrics == port:
        metrics = free_port()
    address = ("127.0.0.1", port)
    command = [str(binary), "--bind", f"127.0.0.1:{port}",
               "--metrics-bind", f"127.0.0.1:{metrics}",
               "--mode", "sync", "--cross-client-group-commit", "1",
               "--clock-reservation-ms", "1000", "--journal", str(journal),
               "--max-entries", str(ENTRIES), "--max-arena-bytes", str(ARENA_BYTES),
               "--max-connection-bytes", str(CONNECTION_BYTES),
               "--max-journal-bytes", str(JOURNAL_BYTES),
               "--checkpoint-percent", "60", "--checkpoint-retry-ms", "100"]
    with log_path.open("xb") as log:
        proc = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 15
            while True:
                if proc.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Fabric failed readiness: " + log_path.read_text()[-2000:])
                if any(line.startswith(f"READY 127.0.0.1:{port} ") for line in log_path.read_text().splitlines()):
                    break
                time.sleep(.025)
            with socket.create_connection(address, timeout=5) as sock:
                exchange(sock, resp(b"PING"), b"+PONG\r\n")
            yield proc, address, metrics
        finally:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=10)


def worker(address, client, start, stop, seconds, rate):
    seq = 0
    hist = Histogram()
    try:
        with socket.create_connection(address, timeout=10) as sock:
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            start.wait(timeout=15)
            begin = time.monotonic()
            while not stop.is_set() and time.monotonic() - begin < seconds:
                wire, reply, next_seq = batch(client, seq)
                batch_start = time.monotonic()
                sent = time.perf_counter_ns()
                exchange(sock, wire, reply)
                hist.add((time.perf_counter_ns() - sent) / 1000)
                seq = next_seq
                # Never catch up in a burst after a storage stall.
                stop.wait(max(0, batch_start + 1 / rate - time.monotonic()))
            return seq, hist
    except BaseException:
        stop.set()
        start.abort()
        raise


def run_soak(binary, root, clients, seconds, rate, report):
    journal = root / "cache.wal"
    stop = threading.Event()
    start = threading.Barrier(clients + 1)
    with instance(binary, journal, root / "write.log") as (proc, address, metrics):
        with ThreadPoolExecutor(max_workers=clients) as pool:
            futures = [pool.submit(worker, address, i, start, stop, seconds, rate)
                       for i in range(clients)]
            try:
                start.wait(timeout=15)
                begin = time.monotonic()
                while not all(f.done() for f in futures):
                    if proc.poll() is not None:
                        raise RuntimeError("Fabric exited during workload")
                    if any(f.done() and f.exception() for f in futures):
                        raise next(f.exception() for f in futures if f.done() and f.exception())
                    sample = scrape(metrics)
                    validate_sample(sample)
                    sample.update(elapsed_seconds=round(time.monotonic() - begin, 3),
                                  rss_bytes=rss_bytes(proc.pid),
                                  actual_journal_bytes=journal.stat().st_size)
                    if sample["actual_journal_bytes"] > JOURNAL_BYTES:
                        raise RuntimeError("actual journal exceeds configured cap")
                    report["samples"].append(sample)
                    if len(report["samples"]) % 5 == 0:
                        print(f"sample {len(report['samples'])}: entries={sample['cache_entries']} "
                              f"checkpoints={sample['checkpoint_completed_total']} rss={sample['rss_bytes']}", flush=True)
                    stop.wait(1)
                results = [f.result() for f in futures]
            finally:
                stop.set()
                start.abort()
        elapsed = time.monotonic() - begin
        sequences = [seq for seq, _ in results]
        if not all(sequences):
            raise RuntimeError("worker completed no commands")
        hist = Histogram()
        for _, h in results:
            hist.merge(h)
        # Stop mutating; allow TTLs and final metrics snapshot to settle.
        time.sleep(1.1)
        final = scrape(metrics)
        validate_sample(final)
        final.update(elapsed_seconds=round(time.monotonic() - begin, 3),
                     rss_bytes=rss_bytes(proc.pid), actual_journal_bytes=journal.stat().st_size)
        report["samples"].append(final)
        if final["actual_journal_bytes"] > JOURNAL_BYTES:
            raise RuntimeError("actual journal exceeds configured cap")
        if final["checkpoint_completed_total"] == 0 or final["cache_expirations_total"] == 0:
            raise RuntimeError("workload did not exercise checkpoint and TTL expiry")
        if final["connections_active"] != 0 or final["connection_budget_used_bytes"] != 0:
            raise RuntimeError("idle connections/buffers were not reclaimed")
        if final["cache_entries"] != 2 * clients:
            raise RuntimeError("expired keys remain physically resident after idle")
        report.update(sequences=sequences, elapsed_seconds=elapsed,
                      commands=4 * sum(sequences),
                      ops_per_second=4 * sum(sequences) / elapsed,
                      latency=hist.report(), precrash=final)
        proc.kill()
        proc.wait(timeout=10)
        if proc.returncode != -9:
            raise RuntimeError("expected SIGKILL")
    # A reserved durable clock watermark can delay immediate restart.
    time.sleep(1.1)
    with instance(binary, journal, root / "recover.log") as (_, address, metrics):
        with socket.create_connection(address, timeout=10) as sock:
            for client, seq in enumerate(sequences):
                exchange(sock, resp(b"GET", counter_key(client)), bulk(str(seq).encode()))
                exchange(sock, resp(b"GET", stable_key(client)), bulk(value(seq)))
                for slot in range(TTL_SLOTS):
                    exchange(sock, resp(b"GET", ttl_key(client, slot)), b"$-1\r\n")
                exchange(sock, resp(b"INCR", counter_key(client)), b":%d\r\n" % (seq + 1))
        report["recovery_verified"] = True
    report["passed"] = True


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--fabric", type=Path, required=True)
    p.add_argument("--work-dir", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--clients", type=int, default=8)
    p.add_argument("--seconds", type=int, default=120)
    p.add_argument("--batches-per-second", type=int, default=20)
    a = p.parse_args()
    if not 1 <= a.clients <= 16 or not 10 <= a.seconds <= 3600 or not 1 <= a.batches_per_second <= 100:
        p.error("clients 1..16, duration 10..3600 seconds, rate 1..100 batches/s")
    binary, work = a.fabric.resolve(), a.work_dir.resolve()
    if not binary.is_file() or not work.is_dir() or shutil.disk_usage(work).free < 128 * 1024 * 1024:
        p.error("trusted executable and work directory with 128 MiB free required")
    report = {"passed": False, "binary_sha256": binary_hash(binary),
              "platform": platform.platform(), "clients": a.clients,
              "requested_seconds": a.seconds, "batches_per_second_per_client": a.batches_per_second,
              "pipeline": 16, "ttl_ms": TTL_MS, "ttl_slots_per_client": TTL_SLOTS,
              "limits": {"entries": ENTRIES, "arena_bytes": ARENA_BYTES,
                         "connection_bytes": CONNECTION_BYTES, "journal_bytes": JOURNAL_BYTES},
              "mode": "sync", "cross_client_group_commit": True,
              "clock_reservation_ms": 1000, "checkpoint_percent": 60,
              "checkpoint_retry_ms": 100,
              "samples": [], "recovery_verified": False}
    with a.output.open("x") as f, tempfile.TemporaryDirectory(prefix="fabric-soak-", dir=work) as tmp:
        try:
            run_soak(binary, Path(tmp), a.clients, a.seconds, a.batches_per_second, report)
        except BaseException as error:
            report["failure"] = str(error)
            raise
        finally:
            json.dump(report, f, indent=2)
            f.write("\n")
    print(f"passed: {report['commands']} commands, {report['ops_per_second']:.0f} ops/s", flush=True)


if __name__ == "__main__":
    main()
