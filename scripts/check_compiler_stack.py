#!/usr/bin/env python3
"""Linux regression: language conformance must not require an enlarged process stack.

Usage: python3 scripts/check_compiler_stack.py /path/to/nulang
The binary must be built from the exact revision under test.
"""
import os
from pathlib import Path
import resource
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = (
    "spill_03_wide_spill_correct_sum.nula",
    "spill_04_nested_lets_correct_sum.nula",
)
STACK_BYTES = 8 * 1024 * 1024


def limit_child_stack():
    _, hard = resource.getrlimit(resource.RLIMIT_STACK)
    resource.setrlimit(resource.RLIMIT_STACK, (STACK_BYTES, hard))


def main(binary):
    for fixture in FIXTURES:
        path = ROOT / "conformance" / "behavior" / fixture
        result = subprocess.run(
            [str(Path(binary).resolve()), "--check", str(path)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=30,
            preexec_fn=limit_child_stack,
            check=False,
            cwd=ROOT,
        )
        if result.returncode != 0:
            print(
                f"FAIL {fixture}: exit={result.returncode}\n{result.stderr[-1500:]}",
                file=sys.stderr,
            )
            return 1
        print(f"PASS {fixture} (--check, 8 MiB stack)")
    return 0


if __name__ == "__main__":
    if sys.platform != "linux" or len(sys.argv) != 2:
        print("Usage (Linux only): check_compiler_stack.py /path/to/nulang", file=sys.stderr)
        sys.exit(2)
    sys.exit(main(sys.argv[1]))
