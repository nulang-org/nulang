#!/usr/bin/env python3
"""Exercise built-in host effects with an actor-backed native Nulang program.

This is an end-to-end smoke test for the bytecode CLI, not a proof of general
actor recovery or an endorsement of ambient privileges inside actors.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


PROGRAM = '''persistent actor Probe {
    state durable stage: Int = 0
    behavior read_stage() { self.stage }
}
fn main() {
    let worker = spawn Probe {}
    let stage = ask worker read_stage()
    perform IO.print("STAGE=" + perform Int.to_string(stage))
    let value = EFFECT_CALL
    perform IO.print("RESULT=" + value)
    0
}
'''


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nulang-bin", default="nulang", help="Native full-featured nulang CLI")
    args = parser.parse_args()
    executable = shutil.which(args.nulang_bin)
    if not executable:
        parser.error(f"Nulang executable not found: {args.nulang_bin}")
    executable = str(Path(executable).resolve())

    cases = (
        ("Env.get", 'perform Env.get("NULANG_ACTOR_HOST_EFFECT_SMOKE")', "smoke-from-host"),
        ("System.arg", "perform System.arg(0)", executable),
        ("FS.read", 'perform FS.read("payload.txt")', "smoke-file-content"),
    )
    for name, expression, expected in cases:
        with tempfile.TemporaryDirectory(prefix="nulang-actor-host-") as temp:
            directory = Path(temp)
            source = directory / "probe.nula"
            source.write_text(PROGRAM.replace("EFFECT_CALL", expression), encoding="utf-8")
            (directory / "payload.txt").write_text("smoke-file-content", encoding="utf-8")
            env = os.environ.copy()
            env["NULANG_ACTOR_HOST_EFFECT_SMOKE"] = "smoke-from-host"
            command = [executable, "--backend", "bytecode", "--with", "fs,os",
                       "--store", f"json:{directory / 'store'}", str(source)]
            result = subprocess.run(command, cwd=directory, env=env, capture_output=True,
                                    text=True, timeout=20)
            lines = [line.strip() for line in result.stdout.splitlines()]
            if result.returncode != 0 or "STAGE=0" not in lines or f"RESULT={expected}" not in lines:
                print(f"FAIL {name}: exit={result.returncode}; lines={lines}; "
                      f"stderr={result.stderr.strip()}", file=sys.stderr)
                return 1
            print(f"PASS {name}: actor-backed VM preserved host builtin")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
