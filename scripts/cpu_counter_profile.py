#!/usr/bin/env python3
"""Opt-in, Linux-only perf stat capture for already-built benchmark commands.

No shell invocation, no sudo, no workload discovery, no automatic publishing.
Run only with a repeatable, non-destructive command whose side effects are safe.
"""

import argparse
import json
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys

EVENTS = (
    "cycles",
    "instructions",
    "branches",
    "branch-misses",
    "cache-references",
    "cache-misses",
)
COUNT = re.compile(r"^[0-9]+(?:\.[0-9]+)?$")


def parse_stat(content):
    """Parse perf's semicolon-delimited counter records, never estimates."""
    counters = {}
    for line in content.splitlines():
        fields = [field.strip() for field in line.split(";")]
        if len(fields) < 3 or fields[2] not in EVENTS:
            continue
        raw = fields[0]
        if COUNT.fullmatch(raw):
            counters[fields[2]] = int(raw) if raw.isdigit() else float(raw)
        else:
            counters[fields[2]] = None  # not supported / not counted
    return counters


def cpu_model():
    try:
        for line in pathlib.Path("/proc/cpuinfo").read_text().splitlines():
            if line.lower().startswith("model name"):
                return line.partition(":")[2].strip()
    except OSError:
        pass
    return "unknown"


def repo_sha(repo_root):
    result = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "HEAD"],
        capture_output=True, text=True, check=False,
    )
    return result.stdout.strip() if result.returncode == 0 else None


def collect(output, command, repetitions, warmups, repo_root):
    if sys.platform != "linux":
        raise RuntimeError("Linux perf is required; no non-Linux fallback is claimed")
    perf = shutil.which("perf")
    if not perf:
        raise RuntimeError("perf is required (install linux-tools/perf and configure perf_event permissions)")
    if output.exists():
        raise RuntimeError(f"output already exists (refusing overwrite): {output}")
    output.mkdir(parents=True)
    version = subprocess.run([perf, "--version"], capture_output=True, text=True, check=False)
    manifest = {
        "schema_version": 1,
        "source_sha": repo_sha(repo_root),
        "command": command,
        "hardware": {
            "machine": platform.machine(),
            "cpu_model": cpu_model(),
            "kernel": platform.release(),
            "affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
            "perf_version": version.stdout.strip(),
        },
        "events": list(EVENTS),
        "warmups": warmups,
        "repetitions": repetitions,
        "runs": [],
    }
    env = dict(os.environ, LC_ALL="C")
    try:
        for _ in range(warmups):
            result = subprocess.run(command, env=env, check=False)
            if result.returncode:
                raise RuntimeError(f"warmup command failed with exit {result.returncode}")
        for index in range(repetitions):
            stats = output / f"run-{index + 1:02d}.perf.txt"
            with (output / f"run-{index + 1:02d}.stdout.txt").open("w") as stdout, \
                 (output / f"run-{index + 1:02d}.stderr.txt").open("w") as stderr:
                result = subprocess.run(
                    [perf, "stat", "--no-big-num", "-x", ";", "-e", ",".join(EVENTS),
                     "-o", str(stats), "--", *command],
                    stdout=stdout, stderr=stderr, env=env, check=False,
                )
            counts = parse_stat(stats.read_text() if stats.exists() else "")
            row = {"exit_code": result.returncode, "counters": counts}
            manifest["runs"].append(row)
            if result.returncode:
                raise RuntimeError(f"perf/workload exited {result.returncode}; see run-{index + 1:02d}.stderr.txt")
            if counts.get("cycles") is None or counts.get("instructions") is None:
                raise RuntimeError("cycles/instructions unavailable; check perf_event permissions and hardware support")
    finally:
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return manifest


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=pathlib.Path, help="new output directory; never overwritten")
    parser.add_argument("--repeat", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("command", nargs=argparse.REMAINDER, help="-- executable and arguments")
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else []
    if not command or not 1 <= args.repeat <= 20 or not 0 <= args.warmup <= 20:
        parser.error("provide -- COMMAND, --repeat 1..20, and --warmup 0..20")
    try:
        collect(args.output, command, args.repeat, args.warmup, pathlib.Path(__file__).resolve().parent.parent)
    except (OSError, RuntimeError) as exc:
        parser.exit(1, f"cpu counter profiling failed: {exc}\n")


if __name__ == "__main__":
    main()
