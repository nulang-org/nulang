#!/usr/bin/env python3
"""Compare clean and no-op Cargo builds with two isolated development profiles.

Run on the same pinned toolchain and machine; this does not modify source files.
Results are indicative, not controlled performance-regression evidence.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time


PROFILES = ("dev", "dev-fast")


def measure(repo_root: Path, targets_root: Path) -> dict:
    """Build each profile twice, with separate cold Cargo target directories."""
    results = {}
    for profile in PROFILES:
        target = targets_root / profile
        target.mkdir(parents=True, exist_ok=False)
        cmd = [
            "cargo", "build", "--locked", "--no-default-features",
            "--bin", "nulang", "--profile", profile, "--timings",
        ]
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(target)
        samples = []
        for stage in ("cold", "warm"):
            print(f"[{profile}] {stage} build: {' '.join(cmd)}", flush=True)
            started = time.perf_counter()
            subprocess.run(cmd, cwd=repo_root, env=env, check=True)
            samples.append(round(time.perf_counter() - started, 3))
        results[profile] = {
            "command": cmd,
            "cold_seconds": samples[0],
            "warm_seconds": samples[1],
        }
    return results


def write_report(path: Path, report: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(report, sort_keys=True, indent=2) + "\n", encoding="utf-8")


def version(args: list[str], cwd: Path) -> str:
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output", required=True, type=Path,
        help="JSON report path (kept even after the disposable target directories are removed)",
    )
    args = parser.parse_args()
    repo_root = Path(__file__).resolve().parent.parent
    # Record the revision and toolchain so measurements are reproducible.
    metadata = {
        "revision": version(["git", "rev-parse", "HEAD"], repo_root),
        "rustc": version(["rustc", "--version"], repo_root),
        "cargo": version(["cargo", "--version"], repo_root),
        "machine": platform.platform(),
        "cpu_count": os.cpu_count(),
        "cargo_incremental": os.environ.get("CARGO_INCREMENTAL"),
        "rustflags": os.environ.get("RUSTFLAGS"),
        "method": "one clean and one no-op build per profile, same feature set, separate disposable targets",
    }
    with tempfile.TemporaryDirectory(prefix="nulang-profile-bench-") as temp:
        measurements = measure(repo_root, Path(temp))
    report = {"metadata": metadata, "measurements": measurements}
    write_report(args.output, report)
    print(f"Wrote {args.output} (all builds successful)")
    print("Note: OS filesystem caches and run order are not controlled; confirm with repeated runs.")


if __name__ == "__main__":
    main()
