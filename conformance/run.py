#!/usr/bin/env python3
import os, subprocess, json, sys

os.chdir(os.path.dirname(os.path.abspath(__file__)) + "/..")

# Select real .nula/.json fixture pairs before building the compiler.
# A misspelled prefix must fail, not silently report zero tests passed.
prefixes = tuple(sys.argv[1:])
selected = sorted(
    file for file in os.listdir("conformance/behavior")
    if file.endswith(".nula")
    and (not prefixes or file.startswith(prefixes))
    and os.path.isfile(f"conformance/behavior/{file[:-5]}.json")
)
if not selected:
    print(f"No conformance fixtures matched: {prefixes or ('all',)}", file=sys.stderr)
    sys.exit(1)

missing_prefixes = [
    prefix for prefix in prefixes
    if not any(file.startswith(prefix) for file in selected)
]
if missing_prefixes:
    print(f"No conformance fixtures matched: {missing_prefixes}", file=sys.stderr)
    sys.exit(1)

subprocess.run(["cargo", "build", "--quiet"], check=True)
# Resolve the binary through cargo so the repo's `target-dir` override in
# `.cargo/config.toml` (currently `/tmp/cargo-target`) is respected — a
# hardcoded `./target/debug/nulang` runs a stale or missing binary.
target_dir = json.loads(
    subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--no-deps"])
)["target_directory"]
nulang_bin = os.path.join(target_dir, "debug", "nulang")

passed = 0
failed = 0

# Optional basename prefixes select a focused conformance subset; no args
# retain the full-suite behavior used by existing manual invocations.
for file in selected:
    base = file[:-5]
    json_path = f"conformance/behavior/{base}.json"
    nula_path = f"conformance/behavior/{file}"
    
    with open(json_path) as f:
        expected = json.load(f)
        
    res = subprocess.run([nulang_bin, nula_path], capture_output=True, text=True)
    out = res.stdout.strip()
    err = res.stderr.strip()
    code = res.returncode
    
    exp_code = expected.get("exit_code", 0)
    exp_out = expected.get("stdout", "")
    exp_err = expected.get("stderr", "")
    
    if code != exp_code or out != exp_out or (exp_err and exp_err not in err):
        print(f"FAIL: {file}")
        if code != exp_code: print(f"  Code: expected {exp_code}, got {code}")
        if out != exp_out: print(f"  Out: expected {exp_out!r}, got {out!r}")
        if exp_err and exp_err not in err: print(f"  Err: expected to contain {exp_err!r}, got {err!r}")
        failed += 1
    else:
        print(f"PASS: {file}")
        passed += 1

print(f"\n{passed} passed, {failed} failed.")
if failed > 0: sys.exit(1)
