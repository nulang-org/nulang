#!/usr/bin/env python3
"""Bisect the prepped compile_hex source by top-level let bindings."""
import subprocess, sys, os

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NULANG = os.environ.get("NULANG_BIN", "cargo run --quiet --bin nulang --")

def run(cmd, stdin=None, timeout=120, text=True):
    proc = subprocess.run(cmd, input=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=text, cwd=REPO_ROOT, timeout=timeout, shell=isinstance(cmd, str))
    return proc.returncode, proc.stdout, proc.stderr

prepped = open(os.path.join(REPO_ROOT, "bootstrap", "prep_compile_hex.txt")).read().strip()

# Split top-level let chain. The prepped output is a single line of the form:
# let a = ... in let b = ... in let c = ... in expr
# We must only split at top-level " in let " boundaries, not nested ones.
def split_top_level_lets(src):
    depth = 0
    in_str = False
    parts = []
    start = 0
    i = 0
    while i < len(src):
        ch = src[i]
        if ch == '"':
            in_str = not in_str
            i += 1
            continue
        if in_str:
            i += 1
            continue
        if ch in '({[':
            depth += 1
            i += 1
            continue
        if ch in ')}]':
            depth -= 1
            i += 1
            continue
        if depth == 0 and src.startswith(" in let ", i):
            parts.append(src[start:i])
            start = i + 4  # keep "let ..."
            i += 7
            continue
        i += 1
    parts.append(src[start:])
    return parts

parts = split_top_level_lets(prepped)
bindings = parts
# The last binding contains the final expression after its " in ".
# We want to replace that final expression with a simple main.

def make_source(n):
    # Take first n bindings, then append a simple main expression.
    src_bindings = bindings[:n]
    # Remove trailing final expr from last binding and append main.
    last = src_bindings[-1]
    # Find the last " in " at top level to separate binding body from final expr.
    depth = 0
    in_str = False
    last_in = -1
    for i, ch in enumerate(last):
        if ch == '"':
            in_str = not in_str
        elif not in_str:
            if ch in '({[':
                depth += 1
            elif ch in ')}]':
                depth -= 1
            elif ch == ' ' and last[i:i+4] == ' in ' and depth == 0:
                last_in = i
    if last_in >= 0:
        last_binding = last[:last_in]
    else:
        last_binding = last
    src_bindings[-1] = last_binding
    main_expr = "123"
    return " in ".join(src_bindings) + " in " + main_expr

def test_n(n):
    src = make_source(n)
    rc, hex_out, err = run(f"{NULANG} bootstrap/compile_hex.nula", stdin=src)
    if rc != 0:
        return f"compile_error rc={rc} stderr={err.strip()[-200:]}"
    rc2, _, err2 = run("python3 bootstrap/fixup_hex.py | python3 bootstrap/hex2nbc.py > bootstrap/bisect_test.nbc",
                        stdin=hex_out)
    if rc2 != 0:
        return f"fixup_error rc={rc2} stderr={err2.strip()[-200:]}"
    rc3, out3, err3 = run(f"{NULANG} bootstrap/bisect_test.nbc")
    last = out3.strip().split("\n")[-1]
    return last

if __name__ == "__main__":
    total = len(bindings)
    print(f"Total bindings: {total}")
    if len(sys.argv) > 1:
        n = int(sys.argv[1])
        print(f"n={n}: {test_n(n)}")
    else:
        # Bisect
        lo, hi = 1, total
        # First find a failing n
        while lo <= hi and test_n(lo).isdigit():
            lo += 1
        print(f"First failing index: {lo}")
        if lo <= total:
            print(f"Result at {lo}: {test_n(lo)}")
            if lo > 1:
                print(f"Result at {lo-1}: {test_n(lo-1)}")
