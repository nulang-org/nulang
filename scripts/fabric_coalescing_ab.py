#!/usr/bin/env python3
"""Compare bounded cross-client collection windows, same binary and disk."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import tempfile

from fabric_soak import binary_hash, run_soak
from fabric_wal_storage_ab import summarize, fs_type


def case_order(windows, round_index):
    return list(windows if round_index % 2 == 0 else reversed(windows))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--fabric", type=Path, required=True)
    p.add_argument("--work-dir", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--wait-us", type=int, action="append")
    p.add_argument("--rounds", type=int, default=2)
    p.add_argument("--seconds", type=int, default=12)
    p.add_argument("--clients", type=int, default=16)
    p.add_argument("--batches-per-second", type=int, default=20)
    a = p.parse_args()
    windows = a.wait_us or [0, 500, 2000]
    if len(set(windows)) != len(windows) or 0 not in windows or len(windows) < 2 or any(not 0 <= w <= 2000 for w in windows):
        p.error("unique windows 0..2000 us, including zero and at least one nonzero")
    if not 1 <= a.rounds <= 10 or not 10 <= a.seconds <= 600 or not 1 <= a.clients <= 16 or not 1 <= a.batches_per_second <= 100:
        p.error("invalid bounded workload")
    binary, work = a.fabric.resolve(), a.work_dir.resolve()
    if not binary.is_file() or not work.is_dir() or shutil.disk_usage(work).free < 128 * 1024 * 1024:
        p.error("trusted executable and work directory with 128 MiB free required")
    filesystem = fs_type(work)
    if filesystem in ("tmpfs", "ramfs"):
        p.error("disk-backed directory required")
    report = {"passed": False, "binary_sha256": binary_hash(binary), "platform": platform.platform(),
              "filesystem": filesystem, "windows_us": windows, "rounds": a.rounds, "seconds": a.seconds,
              "clients": a.clients, "batches_per_second_per_client": a.batches_per_second,
              "expected_cases": len(windows) * a.rounds, "cases": []}
    with a.output.open("x") as output:
        try:
            for index in range(a.rounds):
                for wait in case_order(windows, index):
                    case = {"round": index+1, "wait_us": wait, "passed": False,
                            "recovery_verified": False, "samples": [], "host_load_before": os.getloadavg()}
                    report["cases"].append(case)
                    print(f"round {index+1}, window {wait} us", flush=True)
                    with tempfile.TemporaryDirectory(prefix="fabric-coalesce-ab-", dir=work) as tmp:
                        run_soak(binary, Path(tmp), a.clients, a.seconds, a.batches_per_second, case, True, "cross", wait)
                    summary = summarize(case)
                    summary["coalescing_polls"] = case["precrash"]["coalescing_polls_total"]
                    summary["coalescing_turns"] = case["precrash"]["coalescing_turns_total"]
                    if not wait and (summary["coalescing_polls"] or summary["coalescing_turns"]):
                        raise RuntimeError("zero-wait baseline collected extra readiness")
                    case["summary"] = summary
                    print(json.dumps(summary), flush=True)
            report["passed"] = len(report["cases"]) == report["expected_cases"]
        except BaseException as error:
            report["failure"] = str(error)
            raise
        finally:
            json.dump(report, output, indent=2)
            output.write("\n")


if __name__ == "__main__":
    main()
