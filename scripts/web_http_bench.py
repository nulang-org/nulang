#!/usr/bin/env python3
"""Bounded, loopback-only WebDevServer HTTP benchmark (not a production load tester)."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
import http.client
import json
import math
from pathlib import Path
import sys
import time


LOOPBACK = ("127.0.0.1", "::1", "localhost")


@dataclass(frozen=True)
class Observation:
    latency_ns: int
    status: int | None
    body_bytes: int
    error: str | None = None


def request_once(host: str, port: int, path: str, timeout: float,
                 etag: str | None = None) -> tuple[Observation, str | None]:
    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    started = time.perf_counter_ns()
    try:
        headers = {"Connection": "close"}
        if etag is not None:
            headers["If-None-Match"] = etag
        conn.request("GET", path, headers=headers)
        response = conn.getresponse()
        body = response.read()
        elapsed = time.perf_counter_ns() - started
        return Observation(elapsed, response.status, len(body)), response.getheader("ETag")
    except (OSError, http.client.HTTPException) as error:
        elapsed = time.perf_counter_ns() - started
        return Observation(elapsed, None, 0, type(error).__name__), None
    finally:
        conn.close()


def percentile_ns(samples: list[int], fraction: float) -> int:
    if not samples:
        return 0
    # Nearest rank, no statistical assumptions or interpolation.
    rank = max(1, math.ceil(len(samples) * fraction))
    return sorted(samples)[min(rank, len(samples)) - 1]


def run_phase(host: str, port: int, path: str, count: int, concurrency: int,
              timeout: float, etag: str | None) -> tuple[list[Observation], float]:
    if count == 0:
        return [], 0.0
    workers = min(count, concurrency)
    per_worker = [count // workers + int(i < count % workers) for i in range(workers)]

    def worker(n: int) -> list[Observation]:
        return [request_once(host, port, path, timeout, etag)[0] for _ in range(n)]

    started = time.perf_counter()
    with ThreadPoolExecutor(max_workers=workers) as pool:
        groups = list(pool.map(worker, per_worker))
    elapsed = time.perf_counter() - started
    return [observation for group in groups for observation in group], elapsed


def summarize(observations: list[Observation], elapsed: float,
              expected_status: int) -> dict:
    statuses = Counter(str(obs.status) if obs.status is not None else "transport_error"
                       for obs in observations)
    failures = sum(obs.status != expected_status or obs.error is not None
                   for obs in observations)
    successful = [obs.latency_ns for obs in observations
                  if obs.status == expected_status and obs.error is None]
    return {
        "requests": len(observations),
        "expected_status": expected_status,
        "failures": failures,
        "statuses": dict(sorted(statuses.items())),
        "wall_seconds": round(elapsed, 6),
        "throughput_rps": round(len(observations) / elapsed, 2) if elapsed > 0 else 0,
        "latency_ms_p50": round(percentile_ns(successful, 0.50) / 1e6, 3),
        "latency_ms_p95": round(percentile_ns(successful, 0.95) / 1e6, 3),
        "latency_ms_max": round(max(successful, default=0) / 1e6, 3),
        "body_bytes_total": sum(obs.body_bytes for obs in observations),
        "error_types": dict(sorted(Counter(obs.error for obs in observations if obs.error).items())),
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1", choices=LOOPBACK)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--path", default="/")
    parser.add_argument("--requests", type=int, default=200)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--warmup", type=int, default=20)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--revalidate", action="store_true",
                        help="Fetch one ETag and send If-None-Match on measured GETs")
    parser.add_argument("--expected-status", type=int,
                        help="Defaults to 304 for revalidation, otherwise 200")
    parser.add_argument("--label", default="unlabeled", help="Exact commit/build label")
    parser.add_argument("--output", type=Path, help="Write machine-readable JSON report")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if not (1 <= args.port <= 65535) or not args.path.startswith("/") or args.path.startswith("//"):
        parser.error("port must be 1..65535 and path must begin with a single /")
    if not (1 <= args.requests <= 100000) or not (1 <= args.concurrency <= 64):
        parser.error("requests must be 1..100000 and concurrency must be 1..64")
    if not (0 <= args.warmup <= 100000) or not (0 < args.timeout <= 120):
        parser.error("warmup must be 0..100000 and timeout must be >0 and <=120")
    if args.expected_status is not None and not (100 <= args.expected_status <= 599):
        parser.error("expected status must be an HTTP status (100..599)")

    etag = None
    if args.revalidate:
        preflight, etag = request_once(args.host, args.port, args.path, args.timeout)
        if preflight.status != 200 or not etag:
            print("Revalidation preflight needs HTTP 200 with ETag", file=sys.stderr)
            return 2

    warmup, _ = run_phase(args.host, args.port, args.path, args.warmup,
                          args.concurrency, args.timeout, etag)
    expected = args.expected_status or (304 if args.revalidate else 200)
    if any(obs.status != expected or obs.error for obs in warmup):
        print("Warmup had non-expected responses; measurement aborted", file=sys.stderr)
        return 2
    observations, elapsed = run_phase(args.host, args.port, args.path, args.requests,
                                       args.concurrency, args.timeout, etag)
    report = {"schema": 1, "label": args.label, "host": args.host, "port": args.port,
              "path": args.path, "concurrency": args.concurrency, "warmup": args.warmup,
              "revalidate": args.revalidate, **summarize(observations, elapsed, expected)}
    print(json.dumps(report, indent=2, sort_keys=True))
    if args.output:
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 2 if report["failures"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
