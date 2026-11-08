#!/usr/bin/env python3
"""Run checked-in Nulang language-behavior conformance cases.

Every .nula fixture must have exactly one .json expectation; a missing or empty
suite is an error, not a green CI result. This module is importable without
building Rust so its discovery/runner contracts can be unit tested cheaply.
"""

import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
BEHAVIOR = ROOT / "conformance" / "behavior"


class SuiteError(Exception):
    """The conformance suite cannot be evaluated safely."""


def discover_fixtures(directory):
    """Return deterministic source/expectation pairs or reject incomplete suites."""
    directory = Path(directory)
    if not directory.is_dir():
        raise SuiteError(f"missing behavior fixture directory: {directory}")

    sources = sorted(directory.glob("*.nula"))
    expectations = sorted(directory.glob("*.json"))
    source_names = {path.stem for path in sources}
    expectation_names = {path.stem for path in expectations}
    missing = sorted(source_names - expectation_names)
    orphan = sorted(expectation_names - source_names)
    if missing:
        raise SuiteError(f"missing .json expectations for: {', '.join(missing)}")
    if orphan:
        raise SuiteError(f"orphan .json expectations without .nula: {', '.join(orphan)}")
    if not sources:
        raise SuiteError(f"no .nula fixtures found in {directory}")

    return [(path, directory / f"{path.stem}.json") for path in sources]


def load_expectation(path):
    with path.open(encoding="utf-8") as stream:
        expected = json.load(stream)
    if not isinstance(expected, dict):
        raise ValueError("expected a JSON object")
    code = expected.get("exit_code", 0)
    stdout = expected.get("stdout", "")
    stderr = expected.get("stderr", "")
    if isinstance(code, bool) or not isinstance(code, int):
        raise ValueError("exit_code must be an integer")
    if not isinstance(stdout, str) or not isinstance(stderr, str):
        raise ValueError("stdout and stderr must be strings")
    return code, stdout, stderr


def run_suite(nulang_binary, fixtures, working_dir, timeout_seconds=60):
    """Execute cases. Return (passed, failed), without hiding failed fixtures."""
    passed = failed = 0
    for source, specification in fixtures:
        try:
            exp_code, exp_out, exp_err = load_expectation(specification)
            result = subprocess.run(
                [str(nulang_binary), str(source)],
                cwd=working_dir,
                capture_output=True,
                text=True,
                timeout=timeout_seconds,
            )
        except (ValueError, OSError, json.JSONDecodeError) as exc:
            print(f"FAIL: {source.name} (invalid fixture or execution: {exc})")
            failed += 1
            continue
        except subprocess.TimeoutExpired:
            print(f"FAIL: {source.name} (exceeded {timeout_seconds}s timeout)")
            failed += 1
            continue

        out = result.stdout.strip()
        err = result.stderr.strip()
        mismatches = []
        if result.returncode != exp_code:
            mismatches.append(f"Code: expected {exp_code}, got {result.returncode}")
        if out != exp_out:
            mismatches.append(f"Out: expected {exp_out!r}, got {out!r}")
        # Existing conformance semantics permit extra diagnostic stderr when
        # no substring is requested; preserve this for current runtime cases.
        if exp_err and exp_err not in err:
            mismatches.append(f"Err: expected to contain {exp_err!r}, got {err!r}")
        if mismatches:
            print(f"FAIL: {source.name}")
            for mismatch in mismatches:
                print(f"  {mismatch}")
            failed += 1
        else:
            print(f"PASS: {source.name}")
            passed += 1
    print(f"\n{passed} passed, {failed} failed.")
    return passed, failed


def main():
    try:
        # Reject vacuous/incomplete suites *before* the expensive Rust build.
        fixtures = discover_fixtures(BEHAVIOR)
        subprocess.run(["cargo", "build", "--quiet"], cwd=ROOT, check=True)
        metadata = json.loads(
            subprocess.check_output(
                ["cargo", "metadata", "--format-version", "1", "--no-deps"],
                cwd=ROOT,
            )
        )
        nulang_binary = Path(metadata["target_directory"]) / "debug" / "nulang"
        if not nulang_binary.is_file():
            raise SuiteError(f"compiled Nulang binary missing: {nulang_binary}")
        passed, failed = run_suite(nulang_binary, fixtures, ROOT)
        return 1 if failed or not passed else 0
    except (SuiteError, OSError, subprocess.CalledProcessError, ValueError, KeyError) as exc:
        print(f"conformance runner failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
