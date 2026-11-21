#!/usr/bin/env python3
"""Exercise durable actor state across two separate Nulang CLI processes.

This is a *normal process restart* regression gate, not a SIGKILL, power-loss,
supervisor-restart, replication, or exactly-once external-effect test.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


PROGRAM = """persistent actor DurableCounter {
    state durable value: Int = 0

    behavior increment() {
        self.value = self.value + 1
    }

    behavior current() { self.value }
}

fn main() {
    let c = spawn DurableCounter {} in {
        perform IO.print("RESTORED=" + perform Int.to_string(ask c current()))
        let ignored = ask c increment()
        perform IO.print("SAVED=" + perform Int.to_string(ask c current()))
    }
}
"""


class RecoveryProofError(Exception):
    pass


def run(binary: Path, path: Path, store: Path) -> tuple[int, int]:
    env = dict(os.environ, NULANG_SHARDS="1")
    command = [str(binary), "--store", f"json:{store}", str(path)]
    result = subprocess.run(
        command, capture_output=True, text=True, timeout=45, env=env,
        check=False,
    )
    if result.returncode:
        raise RecoveryProofError(
            f"CLI returned {result.returncode}:\n"
            f"stdout={result.stdout}\nstderr={result.stderr}"
        )
    values = {}
    for name in ("RESTORED", "SAVED"):
        matches = re.findall(rf"(?m)^{name}=([0-9]+)\s*$", result.stdout)
        if len(matches) != 1:
            raise RecoveryProofError(
                f"expected one {name} line, found {matches!r}; "
                f"stdout={result.stdout!r}; stderr={result.stderr!r}"
            )
        values[name] = int(matches[0])
    return values["RESTORED"], values["SAVED"]


def verify(binary: Path) -> None:
    binary = binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="nulang-persistent-restart-") as temp:
        root = Path(temp)
        source = root / "counter.nula"
        store = root / "store"
        source.write_text(PROGRAM, encoding="utf-8")

        first = run(binary, source, store)
        if first != (0, 1):
            raise RecoveryProofError(f"initial process expected (0, 1), got {first}")
        second = run(binary, source, store)
        if second != (1, 2):
            raise RecoveryProofError(
                f"process restart expected restored 1 then saved 2, got {second}"
            )
        print("PASS: fresh process 0->1; second process restores 1->2")
        print("Scope: deterministic single-actor, single-node, file-backed normal exit.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    try:
        verify(args.binary)
    except (RecoveryProofError, OSError, subprocess.TimeoutExpired) as exc:
        print(f"FAIL: CLI durable restart proof: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
