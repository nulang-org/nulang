#!/usr/bin/env python3
"""Run changed .nula behavior fixtures using the already-built Nulang binary.

Unlike conformance/run.py, this does not rebuild the compiler or execute every
behavior fixture. Its comparison semantics deliberately match conformance/run.py.
"""

import argparse
import json
from pathlib import Path
import subprocess
import sys


def cargo_binary(root):
    result = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=root,
        text=True,
    )
    return Path(json.loads(result)["target_directory"]) / "debug" / "nulang"


def run_fixture(root, binary, relative):
    fixture_root = (root / "conformance" / "behavior").resolve()
    fixture = (root / relative).resolve()
    if fixture.suffix != ".nula" or not fixture.is_relative_to(fixture_root):
        return False, f"outside conformance/behavior: {relative}"
    expectation = fixture.with_suffix(".json")
    if not fixture.is_file():
        if not expectation.is_file():
            # A PR may intentionally remove an obsolete fixture and its
            # expectation together; neither can produce useful test output.
            return None, "deleted source and expectation"
        return False, f"missing fixture: {relative}"
    if not expectation.is_file():
        return False, f"missing expectation: {expectation.relative_to(root)}"
    try:
        expected = json.loads(expectation.read_text(encoding="utf-8"))
    except (ValueError, OSError) as error:
        return False, f"bad expectation: {error}"
    if not isinstance(expected, dict):
        return False, "invalid expectation: JSON root must be an object"
    exp_code = expected.get("exit_code", 0)
    exp_stdout = expected.get("stdout", "")
    exp_stderr = expected.get("stderr", "")
    if type(exp_code) is not int or not isinstance(exp_stdout, str) or not isinstance(exp_stderr, str):
        return False, "invalid expectation: exit_code must be an integer; stdout and stderr must be strings"

    try:
        proc = subprocess.run(
            [str(binary), str(fixture)], cwd=root,
            capture_output=True, text=True, timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return False, f"failed to run fixture: {error}"

    actual_stdout = proc.stdout.strip()
    actual_stderr = proc.stderr.strip()
    if proc.returncode != exp_code or actual_stdout != exp_stdout or (
        exp_stderr and exp_stderr not in actual_stderr
    ):
        return False, (
            f"exit expected {exp_code}, actual {proc.returncode}; "
            f"stdout expected {exp_stdout!r}, actual {actual_stdout!r}; "
            f"stderr expectation {exp_stderr!r}, actual {actual_stderr!r}"
        )
    return True, ""


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--file-list", type=Path, required=True)
    args = parser.parse_args(argv)
    root = args.root.resolve()
    changed = [line.strip() for line in args.file_list.read_text(encoding="utf-8").splitlines() if line.strip()]
    # Changing a .json snapshot must rerun its .nula fixture too.
    # Keep each fixture to one execution even if both files changed.
    paths = list(dict.fromkeys(
        str(Path(line).with_suffix(".nula")) if Path(line).suffix == ".json" else line
        for line in changed
    ))
    if not paths:
        print("No changed Nulang behavior fixtures")
        return 0
    binary = args.binary.resolve() if args.binary else cargo_binary(root)
    if not binary.is_file():
        print(f"Nulang binary missing: {binary}")
        return 1
    passed = failed = skipped = 0
    for relative in paths:
        ok, detail = run_fixture(root, binary, relative)
        if ok is None:
            print(f"SKIP: {relative}: {detail}")
            skipped += 1
        elif ok:
            print(f"PASS: {relative}")
            passed += 1
        else:
            print(f"FAIL: {relative}: {detail}")
            failed += 1
    print(f"{passed} passed, {failed} failed, {skipped} skipped")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
