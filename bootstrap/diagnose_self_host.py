#!/usr/bin/env python3
"""diagnose_self_host.py — Diagnostic helper for the Stage 2 self-host pipeline.

Runs the full self-compile oracle and captures the state at each stage:
  1. prep_core.py output (size, first/last chars, line count)
  2. host-compiled compile_hex.nula hex output (line count, first/last lines)
  3. self_compile.nbc metadata (size, function count from bytecode if possible)
  4. self-hosted compiler run on a smoke input (raw output, result of downstream
     fixup+hex2nbc+VM run)
  5. optional NULANG_TRACE=1 snippet around the self-hosted run

Usage:
    python3 bootstrap/diagnose_self_host.py [smoke_expr]

Defaults:
    smoke_expr = "1 + 2 * 3"
"""

import os
import subprocess
import sys
import tempfile

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NULANG = os.environ.get("NULANG_BIN", "cargo run --quiet --bin nulang --")


def run(cmd, stdin=None, cwd=REPO_ROOT, timeout=120, text=True):
    """Run a shell command and return (returncode, stdout, stderr)."""
    proc = subprocess.run(
        cmd,
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=text,
        cwd=cwd,
        timeout=timeout,
        shell=isinstance(cmd, str),
    )
    return proc.returncode, proc.stdout, proc.stderr


def section(title):
    print("")
    print("=" * 60)
    print(title)
    print("=" * 60)


def main():
    smoke_expr = sys.argv[1] if len(sys.argv) > 1 else "1 + 2 * 3"
    section("Configuration")
    print(f"NULANG_BIN={NULANG}")
    print(f"smoke_expr={smoke_expr!r}")

    # Stage 1: prep_core.py output
    section("Stage 1: prep_core.py output")
    rc, prep, err = run(
        "python3 bootstrap/prep_core.py < bootstrap/compile_hex.nula",
        cwd=REPO_ROOT,
    )
    print(f"prep_core exit={rc}")
    if rc != 0:
        print("STDERR:", err[-2000:])
        return 1
    print(f"prep output length={len(prep)} chars, lines={prep.count(chr(10)) + 1}")
    print(f"prep output first 200 chars: {prep[:200]!r}")
    print(f"prep output last 200 chars: {prep[-200:]!r}")

    # Stage 2: host-compile prepped source through compile_hex.nula
    section("Stage 2: host compile prepped source -> hex")
    rc, hex_out, err = run(
        f"{NULANG} bootstrap/compile_hex.nula",
        stdin=prep,
        cwd=REPO_ROOT,
    )
    print(f"host compile exit={rc}")
    if rc != 0:
        print("STDERR:", err[-2000:])
        return 1
    hex_lines = hex_out.strip().splitlines()
    print(f"hex output lines={len(hex_lines)}")
    print("first 10 hex lines:")
    for line in hex_lines[:10]:
        print(f"  {line}")
    print("last 10 hex lines:")
    for line in hex_lines[-10:]:
        print(f"  {line}")

    # Stage 3: fixup + hex2nbc -> self_compile.nbc
    section("Stage 3: fixup + hex2nbc -> self_compile.nbc")
    self_compile_path = os.path.join(REPO_ROOT, "bootstrap", "self_compile.nbc")
    rc, _, err = run(
        f"python3 bootstrap/fixup_hex.py | python3 bootstrap/hex2nbc.py > {self_compile_path}",
        stdin=hex_out,
        cwd=REPO_ROOT,
    )
    print(f"fixup_hex | hex2nbc exit={rc}")
    if rc != 0:
        print("STDERR:", err[-2000:])
        return 1
    nbc_size = os.path.getsize(self_compile_path)
    print(f"self_compile.nbc size={nbc_size} bytes")

    # Stage 4: run self-hosted compiler on smoke expression
    section("Stage 4: self-hosted compiler on smoke expression")
    rc, self_hex, err = run(
        f"{NULANG} bootstrap/self_compile.nbc",
        stdin=smoke_expr,
        cwd=REPO_ROOT,
    )
    print(f"self-hosted compile exit={rc}")
    self_lines = self_hex.strip().splitlines()
    print(f"self-hosted hex output lines={len(self_lines)}")
    print("first 20 self-hosted hex lines:")
    for line in self_lines[:20]:
        print(f"  {line}")
    print("last 10 self-hosted hex lines:")
    for line in self_lines[-10:]:
        print(f"  {line}")

    # Stage 5: downstream fixup + hex2nbc + VM run
    section("Stage 5: self-hosted output -> oracle .nbc -> VM")
    oracle_path = os.path.join(REPO_ROOT, "bootstrap", "self_oracle.nbc")
    rc, _, err = run(
        f"python3 bootstrap/fixup_hex.py | python3 bootstrap/hex2nbc.py > {oracle_path}",
        stdin=self_hex,
        cwd=REPO_ROOT,
    )
    print(f"fixup_hex | hex2nbc exit={rc}")
    if rc != 0:
        print("STDERR:", err[-2000:])
    oracle_size = os.path.getsize(oracle_path)
    print(f"self_oracle.nbc size={oracle_size} bytes")
    rc, result, err = run(
        f"{NULANG} bootstrap/self_oracle.nbc",
        cwd=REPO_ROOT,
    )
    print(f"oracle VM exit={rc}")
    print(f"oracle VM stdout: {result.strip()!r}")
    if err.strip():
        print(f"oracle VM stderr: {err.strip()!r}")

    # Stage 6: trace the self-hosted compiler run (first 50 trace lines)
    section("Stage 6: NULANG_TRACE=1 self-hosted run (first 50 lines)")
    env = os.environ.copy()
    env["NULANG_TRACE"] = "1"
    proc = subprocess.run(
        f"{NULANG} bootstrap/self_compile.nbc",
        input=smoke_expr,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        cwd=REPO_ROOT,
        timeout=60,
        env=env,
        shell=True,
    )
    trace_lines = proc.stdout.splitlines()
    for line in trace_lines[:50]:
        print(line)
    print(f"(... {len(trace_lines)} total trace lines)")

    return 0


if __name__ == "__main__":
    sys.exit(main())
