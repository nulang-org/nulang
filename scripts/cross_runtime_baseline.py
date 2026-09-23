#!/usr/bin/env python3
"""Run a same-host local-mailbox baseline against Rust, Go, and BEAM.

Workload:
  - pre-created mailbox/runtime/process
  - enqueue 100 primitive integer messages
  - drain/dispatch the same 100 messages
  - actor/process construction is outside the timed region

Nulang's number comes from Criterion's actor/message_roundtrip_local/100
benchmark. Rust uses std::sync::mpsc::sync_channel, Go uses a buffered chan,
and BEAM sends to/receives from the current process mailbox. These are not
claims about whole-language performance; they are a deliberately narrow
runtime messaging baseline on the same CI host.
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import tempfile
from pathlib import Path

BATCH = 100
TRIALS = 5

RUST = r'''
use std::sync::mpsc::sync_channel;
use std::time::Instant;

const BATCH: usize = 100;
const ITERS: usize = 50_000;
const WARMUP: usize = 2_000;

fn run(iters: usize) -> u64 {
    let (tx, rx) = sync_channel::<u64>(BATCH);
    let mut checksum = 0u64;
    for _ in 0..iters {
        for i in 0..BATCH {
            tx.send(i as u64).unwrap();
        }
        for _ in 0..BATCH {
            checksum = checksum.wrapping_add(rx.recv().unwrap());
        }
    }
    checksum
}

fn main() {
    std::hint::black_box(run(WARMUP));
    let start = Instant::now();
    let checksum = run(ITERS);
    let ns_per_batch = start.elapsed().as_nanos() as f64 / ITERS as f64;
    let messages_per_sec = 1_000_000_000.0 * BATCH as f64 / ns_per_batch;
    println!(
        "{{\"runtime\":\"rust\",\"ns_per_batch\":{:.3},\"messages_per_sec\":{:.3},\"checksum\":{}}}",
        ns_per_batch, messages_per_sec, checksum
    );
}
'''

GO = r'''
package main

import (
    "fmt"
    "time"
)

const batch = 100
const iters = 50000
const warmup = 2000

func run(n int) uint64 {
    ch := make(chan uint64, batch)
    var checksum uint64
    for iter := 0; iter < n; iter++ {
        for i := 0; i < batch; i++ {
            ch <- uint64(i)
        }
        for i := 0; i < batch; i++ {
            checksum += <-ch
        }
    }
    return checksum
}

func main() {
    _ = run(warmup)
    start := time.Now()
    checksum := run(iters)
    nsPerBatch := float64(time.Since(start).Nanoseconds()) / float64(iters)
    messagesPerSec := 1e9 * float64(batch) / nsPerBatch
    fmt.Printf("{\"runtime\":\"go\",\"ns_per_batch\":%.3f,\"messages_per_sec\":%.3f,\"checksum\":%d}\n",
        nsPerBatch, messagesPerSec, checksum)
}
'''

BEAM = r'''
-module(beam_mailbox).
-export([main/0]).

-define(BATCH, 100).
-define(ITERS, 50000).
-define(WARMUP, 2000).

send_batch(0) -> ok;
send_batch(N) ->
    self() ! N,
    send_batch(N - 1).

recv_batch(0, Acc) -> Acc;
recv_batch(N, Acc) ->
    receive
        Value when is_integer(Value) ->
            recv_batch(N - 1, Acc + Value)
    end.

one_batch() ->
    send_batch(?BATCH),
    recv_batch(?BATCH, 0).

run(0, Acc) -> Acc;
run(N, Acc) ->
    run(N - 1, Acc + one_batch()).

main() ->
    _ = run(?WARMUP, 0),
    T0 = erlang:monotonic_time(nanosecond),
    Checksum = run(?ITERS, 0),
    T1 = erlang:monotonic_time(nanosecond),
    NsPerBatch = (T1 - T0) / ?ITERS,
    MessagesPerSec = 1000000000.0 * ?BATCH / NsPerBatch,
    io:format(
        "{\"runtime\":\"beam\",\"ns_per_batch\":~.3f,\"messages_per_sec\":~.3f,\"checksum\":~p}~n",
        [NsPerBatch, MessagesPerSec, Checksum]
    ).
'''


def run_checked(cmd: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(cmd, cwd=cwd, text=True, capture_output=True, check=True)
    return proc.stdout.strip()


def cargo_target_dir() -> Path:
    data = json.loads(run_checked(["cargo", "metadata", "--no-deps", "--format-version", "1"]))
    return Path(data["target_directory"])


def nulang_result() -> dict:
    estimates = (
        cargo_target_dir()
        / "criterion"
        / "actor"
        / "message_roundtrip_local"
        / "100"
        / "new"
        / "estimates.json"
    )
    data = json.loads(estimates.read_text())
    mean = data["mean"]
    ns = float(mean["point_estimate"])
    ci = mean.get("confidence_interval", {})
    return {
        "runtime": "nulang",
        "ns_per_batch": ns,
        "messages_per_sec": 1_000_000_000.0 * BATCH / ns,
        "ns_lower": ci.get("lower_bound"),
        "ns_upper": ci.get("upper_bound"),
        "trials": None,
    }


def parse_last_json(stdout: str) -> dict:
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if line.startswith("{") and line.endswith("}"):
            return json.loads(line)
    raise RuntimeError(f"no JSON result in output: {stdout!r}")


def median_trials(cmd: list[str], cwd: Path | None = None) -> dict:
    rows = [parse_last_json(run_checked(cmd, cwd=cwd)) for _ in range(TRIALS)]
    ns_values = [float(r["ns_per_batch"]) for r in rows]
    ns = statistics.median(ns_values)
    return {
        "runtime": rows[0]["runtime"],
        "ns_per_batch": ns,
        "messages_per_sec": 1_000_000_000.0 * BATCH / ns,
        "ns_min": min(ns_values),
        "ns_max": max(ns_values),
        "trials": TRIALS,
    }


def tool_version(cmd: list[str]) -> str:
    try:
        return run_checked(cmd).splitlines()[0]
    except Exception as exc:
        return f"unavailable: {exc}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--json-out", type=Path)
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="nulang-cross-runtime-") as tmp_name:
        tmp = Path(tmp_name)
        rust_src = tmp / "rust_mailbox.rs"
        go_src = tmp / "go_mailbox.go"
        beam_src = tmp / "beam_mailbox.erl"
        rust_bin = tmp / "rust_mailbox"
        go_bin = tmp / "go_mailbox"

        rust_src.write_text(RUST)
        go_src.write_text(GO)
        beam_src.write_text(BEAM)

        run_checked(["rustc", "-O", str(rust_src), "-o", str(rust_bin)])
        run_checked(["go", "build", "-o", str(go_bin), str(go_src)])
        run_checked(["erlc", "-o", str(tmp), str(beam_src)])

        results = [
            nulang_result(),
            median_trials([str(rust_bin)]),
            median_trials([str(go_bin)]),
            median_trials(
                ["erl", "-noshell", "-pa", str(tmp), "-s", "beam_mailbox", "main", "-s", "init", "stop"]
            ),
        ]

    payload = {
        "workload": "local mailbox: enqueue 100 primitive integer messages then drain/dispatch 100",
        "batch_size": BATCH,
        "results": results,
        "versions": {
            "rust": tool_version(["rustc", "--version"]),
            "go": tool_version(["go", "version"]),
            "beam": tool_version(
                ["erl", "-noshell", "-eval", 'io:format("OTP ~s~n", [erlang:system_info(otp_release)]), halt().']
            ),
        },
        "caveat": (
            "Nulang includes behavior lookup plus scheduler/native-handler dispatch; "
            "Rust/Go/BEAM baselines exercise their local mailbox/channel primitive. "
            "Use this as a runtime messaging baseline, not a whole-language ranking."
        ),
    }

    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")

    print(json.dumps(payload, sort_keys=True))
    print()
    print("| Runtime | ns / 100 messages | million messages/s |")
    print("|---|---:|---:|")
    for row in results:
        print(
            f"| {row['runtime']} | {row['ns_per_batch']:.1f} | "
            f"{row['messages_per_sec'] / 1_000_000.0:.3f} |"
        )
    print()
    print(payload["caveat"])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
