#!/usr/bin/env python3
import subprocess
import sys
import shlex

REPO = "/home/davidporkka/nulang"
NULANG = "cargo run --quiet --bin nulang --"

def host_eval(expr):
    p = subprocess.run(
        f"{NULANG} -e {shlex.quote(expr)}",
        shell=True, capture_output=True, text=True, cwd=REPO, timeout=120
    )
    if p.returncode != 0:
        return f"HOST_ERROR: {p.stderr.strip()}"
    return p.stdout.strip().splitlines()[-1]

def run_cmd(cmd, stdin=None):
    return subprocess.run(cmd, input=stdin, shell=True, capture_output=True, text=True, cwd=REPO, timeout=120)

def self_eval(expr):
    p = run_cmd("python3 bootstrap/prep_core.py < bootstrap/compile_hex.nula")
    if p.returncode != 0:
        return f"PREP_ERROR: {p.stderr.strip()}"
    prep = p.stdout
    p = run_cmd(f"{NULANG} bootstrap/compile_hex.nula", prep)
    if p.returncode != 0:
        return f"HOST_COMPILE_ERROR: {p.stderr.strip()}"
    hex_out = p.stdout
    p = run_cmd("python3 bootstrap/fixup_hex.py | python3 bootstrap/hex2nbc.py > /tmp/self_compile.nbc", hex_out)
    if p.returncode != 0:
        return f"FIXUP_ERROR: {p.stderr.strip()}"
    p = run_cmd(f"{NULANG} /tmp/self_compile.nbc", expr)
    if p.returncode != 0:
        return f"SELF_COMPILE_ERROR: {p.stderr.strip()}"
    self_hex = p.stdout
    p = run_cmd("python3 bootstrap/fixup_hex.py | python3 bootstrap/hex2nbc.py > /tmp/self_oracle.nbc", self_hex)
    if p.returncode != 0:
        return f"ORACLE_BUILD_ERROR: {p.stderr.strip()}"
    p = run_cmd(f"{NULANG} /tmp/self_oracle.nbc")
    if p.returncode != 0:
        return f"ORACLE_RUN_ERROR: {p.stderr.strip()}"
    return p.stdout.strip().splitlines()[-1]

if __name__ == "__main__":
    expr = sys.argv[1] if len(sys.argv) > 1 else "1 + 2 * 3"
    print("expr:", repr(expr))
    print("host:", host_eval(expr))
    print("self:", self_eval(expr))
