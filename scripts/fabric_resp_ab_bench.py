#!/usr/bin/env python3
"""Paired loopback RESP baseline; validates every reply, reports batch latency.

Use optimized binaries. Run on an otherwise idle host with a real disk-backed
--journal-dir for persistence comparisons. No latency regression gate yet.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import socket
import statistics
import subprocess
import tempfile
import time
from contextlib import contextmanager

VALUE = b"x" * 64
KEY = b"bench:{one}"
OK = b"+OK\r\n"
HIT = b"$64\r\n" + VALUE + b"\r\n"


def resp(*args: bytes) -> bytes:
    return b"*%d\r\n" % len(args) + b"".join(b"$%d\r\n" % len(a) + a + b"\r\n" for a in args)


def percentile(samples: list[float], q: float) -> float:
    return sorted(samples)[max(0, math.ceil(len(samples) * q) - 1)]


def workload(name: str) -> tuple[bytes, bytes]:
    if name in ("get", "ttl-get"):
        return resp(b"GET", KEY), HIT
    if name == "set":
        return resp(b"SET", KEY, VALUE), OK
    if name == "ttl-set":
        return resp(b"SET", KEY, VALUE, b"PX", b"3600000"), OK
    raise ValueError(name)


def read_exact(sock: socket.socket, length: int) -> bytes:
    result = bytearray()
    while len(result) < length:
        data = sock.recv(length - len(result))
        if not data:
            raise RuntimeError("server disconnected before full reply")
        result.extend(data)
    return bytes(result)


def exchange(sock: socket.socket, wire: bytes, expected: bytes) -> None:
    sock.sendall(wire)
    result = read_exact(sock, len(expected))
    if result != expected:
        raise RuntimeError(f"unexpected response: {result[:120]!r}")


def measure(sock: socket.socket, name: str, pipeline: int, commands: int) -> dict:
    if commands < pipeline or commands % pipeline:
        raise ValueError("commands must be a positive multiple of pipeline depth")
    seed = resp(b"SET", KEY, VALUE, b"PX", b"3600000") if name.startswith("ttl-") else resp(b"SET", KEY, VALUE)
    exchange(sock, seed, OK)
    wire, reply = workload(name)
    wire, reply = wire * pipeline, reply * pipeline
    for _ in range(min(8, commands // pipeline)):
        exchange(sock, wire, reply)
    samples = []
    begin = time.perf_counter_ns()
    for _ in range(commands // pipeline):
        start = time.perf_counter_ns()
        exchange(sock, wire, reply)
        samples.append((time.perf_counter_ns() - start) / 1000)
    seconds = (time.perf_counter_ns() - begin) / 1e9
    return {
        "commands": commands, "batches": len(samples), "ops_per_second": commands / seconds,
        "batch_p50_us": percentile(samples, .50), "batch_p95_us": percentile(samples, .95),
        "batch_p99_us": percentile(samples, .99), "batch_p999_us": percentile(samples, .999),
    }


def free_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


@contextmanager
def server(binary: Path, engine: str, mode: str, root: Path):
    with tempfile.TemporaryDirectory(prefix=f"{engine}-{mode}-", dir=root) as tmp:
        path = Path(tmp)
        port = free_port()
        if engine == "fabric":
            command = [str(binary), "--bind", f"127.0.0.1:{port}", "--mode", mode]
            if mode != "memory":
                command += ["--journal", str(path / "cache.wal"), "--checkpoint-percent", "0"]
        else:
            command = [str(binary), "--bind", "127.0.0.1", "--port", str(port),
                       "--save", "", "--appendonly", "no" if mode == "memory" else "yes",
                       "--appendfsync", "always" if mode == "sync" else "everysec",
                       "--auto-aof-rewrite-percentage", "0", "--dir", tmp,
                       "--io-threads", "1", "--protected-mode", "yes"]
        with (path / "server.log").open("wb") as log:
            proc = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
            try:
                until = time.monotonic() + 5
                while True:
                    if proc.poll() is not None:
                        raise RuntimeError(f"server exited: {(path / 'server.log').read_text()[-2000:]}")
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.5) as probe:
                            exchange(probe, resp(b"PING"), b"+PONG\r\n")
                        break
                    except (OSError, RuntimeError):
                        if time.monotonic() >= until:
                            raise RuntimeError("server readiness timed out")
                        time.sleep(.02)
                with socket.create_connection(("127.0.0.1", port), timeout=5) as sock:
                    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                    yield sock
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()


def summarize(samples: list[dict]) -> dict:
    return {key: statistics.median(s[key] for s in samples) for key in samples[0]}


def binary_hash(path: Path) -> str:
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--fabric", required=True, type=Path)
    p.add_argument("--valkey", required=True, type=Path)
    p.add_argument("--journal-dir", required=True, type=Path)
    p.add_argument("--output", required=True, type=Path)
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--memory-commands", type=int, default=4096)
    p.add_argument("--durable-commands", type=int, default=256)
    args = p.parse_args()
    if args.rounds <= 0 or any(n <= 0 or n % 32 for n in (args.memory_commands, args.durable_commands)):
        p.error("rounds must be positive and command counts positive multiples of 32")
    binaries = {"fabric": args.fabric.resolve(), "valkey": args.valkey.resolve()}
    report = {
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "load_average": os.getloadavg(),
        "binary_sha256": {engine: binary_hash(path) for engine, path in binaries.items()},
        "valkey_version": subprocess.check_output([str(binaries["valkey"]), "--version"], text=True).strip(),
        "journal_directory": str(args.journal_dir.resolve()), "rounds": args.rounds,
        "value_bytes": len(VALUE), "clients": 1, "latency_unit": "batch round trip microseconds",
        "cases": [],
    }
    for mode in ("memory", "buffered", "sync"):
        commands = args.memory_commands if mode == "memory" else args.durable_commands
        for name in ("get", "set", "ttl-get", "ttl-set"):
            for depth in (1, 32):
                samples = {engine: [] for engine in binaries}
                for n in range(args.rounds):
                    order = ("fabric", "valkey") if n % 2 == 0 else ("valkey", "fabric")
                    for engine in order:
                        with server(binaries[engine], engine, mode, args.journal_dir) as sock:
                            samples[engine].append(measure(sock, name, depth, commands))
                case = {"mode": mode, "workload": name, "pipeline": depth,
                        "samples": samples, "median": {e: summarize(s) for e, s in samples.items()}}
                report["cases"].append(case)
                print(mode, name, depth, {e: round(s["ops_per_second"]) for e, s in case["median"].items()}, flush=True)
                args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
