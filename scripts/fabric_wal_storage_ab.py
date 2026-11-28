#!/usr/bin/env python3
"""Same-binary, alternating storage/batching comparison. RAM is not durable."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import tempfile
import subprocess

from fabric_soak import run_soak, binary_hash


def case_order(index):
    order = [("disk", "connection"), ("ram", "cross"),
             ("disk", "cross"), ("ram", "connection")]
    return order if index % 2 == 0 else list(reversed(order))


def summarize(report):
    if not report["passed"] or not report["recovery_verified"]:
        raise ValueError("incomplete or failed qualification")
    counters, hists = report["precrash"], report["internal_latency"]
    for operation, counter in (("command", "commands_dispatched_total"),
                               ("wal_sync", "wal_groups_synced_total"),
                               ("checkpoint", "checkpoint_completed_total")):
        if hists[operation]["count"] != counters[counter]:
            raise ValueError(f"histogram mismatch: {operation}")
    syncs = counters["wal_groups_synced_total"]
    if not syncs:
        raise ValueError("no group syncs")
    return {
        "commands": report["commands"],
        "ops_per_second": report["ops_per_second"],
        "batch_p99_upper_us": report["latency"]["p99_upper_us"],
        "batch_max_us": report["latency"]["max_us"],
        "wal_syncs": syncs,
        "records_per_sync": counters["wal_group_records_synced_total"] / syncs,
        "wal_sync_sum_us": hists["wal_sync"]["sum_us"],
        "wal_sync_p99_upper_us": hists["wal_sync"]["p99_upper_us"],
        "checkpoint_sum_us": hists["checkpoint"]["sum_us"],
        "command_sum_us": hists["command"]["sum_us"],
    }


def fs_type(path):
    return subprocess.check_output(["stat", "-f", "-c", "%T", str(path)], text=True).strip()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--fabric", type=Path, required=True)
    p.add_argument("--disk-dir", type=Path, required=True)
    p.add_argument("--ram-dir", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--rounds", type=int, default=2)
    p.add_argument("--seconds", type=int, default=15)
    p.add_argument("--clients", type=int, default=16)
    p.add_argument("--batches-per-second", type=int, default=20)
    a = p.parse_args()
    if not 1 <= a.rounds <= 10 or not 10 <= a.seconds <= 600 or not 1 <= a.clients <= 16 or not 1 <= a.batches_per_second <= 100:
        p.error("invalid bounded workload")
    binary = a.fabric.resolve()
    roots = {"disk": a.disk_dir.resolve(), "ram": a.ram_dir.resolve()}
    if not binary.is_file() or any(not r.is_dir() or shutil.disk_usage(r).free < 128 * 1024 * 1024 for r in roots.values()):
        p.error("trusted executable and two work directories with 128 MiB free required")
    types = {name: fs_type(root) for name, root in roots.items()}
    if types["ram"] != "tmpfs" or types["disk"] in ("tmpfs", "ramfs") or roots["disk"].stat().st_dev == roots["ram"].stat().st_dev:
        p.error("use different filesystems, including a tmpfs RAM control")
    report = {"passed": False, "binary_sha256": binary_hash(binary), "platform": platform.platform(),
              "filesystems": types, "clients": a.clients, "seconds": a.seconds,
              "batches_per_second_per_client": a.batches_per_second,
              "rounds": a.rounds, "expected_cases": 4 * a.rounds, "cases": [],
              "scope": "same-binary closed-loop workload; tmpfs is a volatile negative storage control"}
    with a.output.open("x") as output:
        try:
            for index in range(a.rounds):
                for storage, mode in case_order(index):
                    case = {"round": index + 1, "storage": storage, "commit_mode": mode,
                            "passed": False, "recovery_verified": False, "samples": [],
                            "host_load_before": os.getloadavg()}
                    report["cases"].append(case)
                    print(f"round {index+1}: {storage}/{mode}", flush=True)
                    with tempfile.TemporaryDirectory(prefix="fabric-wal-ab-", dir=roots[storage]) as tmp:
                        run_soak(binary, Path(tmp), a.clients, a.seconds,
                                 a.batches_per_second, case, True, mode)
                    case["summary"] = summarize(case)
                    print(json.dumps(case["summary"]), flush=True)
            report["passed"] = len(report["cases"]) == report["expected_cases"]
        except BaseException as error:
            report["failure"] = str(error)
            raise
        finally:
            json.dump(report, output, indent=2)
            output.write("\n")


if __name__ == "__main__":
    main()
