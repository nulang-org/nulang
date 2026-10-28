#!/usr/bin/env python3
"""Retry only transient RustSec advisory DB fetch errors, never audit findings.

All attempts invoke an ordinary `cargo audit`, without `--no-fetch`, ignore
flags, advisory exceptions, altered exit codes, or skipping the dependency scan.
"""

import subprocess
import sys
import time

MAX_ATTEMPTS = 3
FETCH_FAILURE = "couldn't fetch advisory database"


def run_audit() -> int:
    for attempt in range(1, MAX_ATTEMPTS + 1):
        try:
            result = subprocess.run(
                ['cargo', 'audit'],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
        except OSError as exc:
            print(f'cargo audit could not start: {exc}', file=sys.stderr)
            return 127

        output = result.stdout or ''
        sys.stdout.write(output)
        sys.stdout.flush()
        if result.returncode == 0:
            return 0
        # Strictly retry the expected Git/RustSec availability failure, not
        # vulnerability findings, a malformed lockfile, or tooling failures.
        if FETCH_FAILURE not in output.lower() or attempt == MAX_ATTEMPTS:
            return result.returncode

        print(f'RustSec advisory fetch failed; retrying ({attempt}/{MAX_ATTEMPTS})',
              file=sys.stderr, flush=True)
        time.sleep(1)

    return 1  # unreachable, but fail closed


if __name__ == '__main__':
    sys.exit(run_audit())
