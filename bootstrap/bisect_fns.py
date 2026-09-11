#!/usr/bin/env python3
import subprocess, tempfile, os

def test_n_fns(n):
    lines = [f"fn dummy{i}(x: Int) {{ x }}" for i in range(n)]
    lines += [
        "fn compile(src: String) { perform IO.print(\"; \" + src); 0 }",
        "fn main() { let input = perform IO.read(); compile(input) }",
    ]
    src = "\n".join(lines)
    prep = subprocess.run(["python3","bootstrap/prep_core.py"], input=src, capture_output=True, text=True).stdout
    pipe = subprocess.run(["cargo","run","--quiet","--bin","nulang","--","bootstrap/compile_hex.nula"], input=prep, capture_output=True, text=True)
    if pipe.returncode != 0:
        return "compile_hex_fail"
    fix = subprocess.run(["python3","bootstrap/fixup_hex.py"], input=pipe.stdout, capture_output=True, text=True)
    nbc = subprocess.run(["python3","bootstrap/hex2nbc.py"], input=fix.stdout.encode(), capture_output=True)
    fd, path = tempfile.mkstemp(suffix=".nbc")
    os.write(fd, nbc.stdout); os.close(fd)
    run = subprocess.run(["cargo","run","--quiet","--bin","nulang","--", path], input="1+2", capture_output=True, text=True)
    os.unlink(path)
    return run.stdout.strip().split("\n")[-1]

for n in [0, 2, 5, 10, 15, 20, 25, 30, 35, 40, 42, 44]:
    print(f"n={n}: {test_n_fns(n)}")
