"""Execute all infrastructure Criterion cases once, not a timing benchmark."""

import argparse
import os
from pathlib import Path
import subprocess
import tempfile


EXPECTED = (
    *(f"nudb/snapshot_point_read/{kind}/{depth}"
      for depth in (1, 64, 4096) for kind in ("latest", "oldest")),
    "nudb/memory_prepare_commit/one_mutation",
    *(f"nudb/durable_prepare_commit/single_write/{size}" for size in (64, 4096)),
    *(f"fabric/durable_local_append/existing_stream/{size}" for size in (64, 4096)),
    "fabric/local_replay/head/128",
    "fabric/local_replay/tail/128",
)
FILTER = r"^(nudb/|fabric/(durable_local_append|local_replay)/)"


def verify_output(output):
    """Reject skipped, duplicated or unfinished cases, even on exit zero."""
    seen = set()
    completed = set()
    active = None
    for line in output.splitlines():
        line = line.strip()
        if line.startswith("Testing "):
            if active is not None:
                raise ValueError(f"unfinished case: {active}")
            active = line.removeprefix("Testing ")
            if active not in EXPECTED or active in seen:
                raise ValueError(f"unexpected or duplicate case: {active}")
            seen.add(active)
        elif line == "Success" and active is not None:
            completed.add(active)
            active = None
    if active is not None or completed != set(EXPECTED):
        raise ValueError(f"missing successful cases: {sorted(set(EXPECTED) - completed)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--storage-root", required=True, type=Path,
                        help="existing directory on the filesystem to exercise")
    parser.add_argument("--no-default-features", action="store_true")
    args = parser.parse_args()
    command = ["cargo", "test", "--locked", "--bench", "bench_main"]
    if args.no_default_features:
        command.append("--no-default-features")
    command.extend(["--", "--color", "never", FILTER])
    with tempfile.TemporaryDirectory(prefix="nulang-infra-smoke-",
                                     dir=args.storage_root.resolve()) as root:
        env = dict(os.environ, NULANG_BENCH_STORAGE_DIR=root)
        with subprocess.Popen(command, env=env, text=True, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT) as process:
            lines = []
            for line in process.stdout:
                print(line, end="", flush=True)
                lines.append(line)
            status = process.wait()
        if status:
            raise SystemExit(status)
        verify_output("".join(lines))
        if any(Path(root).iterdir()):
            raise ValueError("benchmark leaked storage fixtures")
    print(f"Verified {len(EXPECTED)} infrastructure smoke cases (no timing claim).")


if __name__ == "__main__":
    main()
