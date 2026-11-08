#!/usr/bin/env python3
"""Run the behavioral contract with per-fixture capabilities and durable state."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
BEHAVIOR = ROOT / "conformance" / "behavior"
ALLOWED_CAPABILITIES = frozenset({"fs", "net", "os"})


def case_command(nulang_bin: str, case: Path, expected: dict, store_root: Path) -> list[str]:
    """Construct an isolated command, without inheriting durable state from peers."""
    case = Path(case)
    if ".." in case.parts or case.suffix != ".nula":
        raise ValueError(f"Invalid conformance case path: {case}")
    caps = expected.get("capabilities", [])
    if not isinstance(caps, list) or not all(
        isinstance(cap, str) and cap in ALLOWED_CAPABILITIES for cap in caps
    ):
        raise ValueError(f"Invalid capabilities for {case.name}: {caps!r}")
    # Unique case stem under a fresh suite-scoped TemporaryDirectory. The
    # runtime may create the directory; it is removed when the suite exits.
    store = Path(store_root) / case.stem
    # Keep the script in argv[1]: System.arg(1) is part of the language
    # contract, even when the runner grants runtime capabilities or store.
    command = [str(nulang_bin), str(case), "--store", str(store)]
    if caps:
        command.extend(["--with", ",".join(caps)])
    return command


def run_case(nulang_bin: str, case: Path, expected: dict, store_root: Path) -> bool:
    cmd = case_command(nulang_bin, case, expected, store_root)
    result = subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT)
    out, err = result.stdout.strip(), result.stderr.strip()
    exp_code = expected.get("exit_code", 0)
    exp_out = expected.get("stdout", "")
    exp_err = expected.get("stderr", "")
    passed = (result.returncode == exp_code and out == exp_out
              and (not exp_err or exp_err in err))
    print(f"{'PASS' if passed else 'FAIL'}: {case.name}")
    if not passed:
        if result.returncode != exp_code:
            print(f"  Code: expected {exp_code}, got {result.returncode}")
        if out != exp_out:
            print(f"  Out: expected {exp_out!r}, got {out!r}")
        if exp_err and exp_err not in err:
            print(f"  Err: expected to contain {exp_err!r}, got {err!r}")
    return passed


def script_argument(case: Path) -> Path:
    """Keep checked-in fixtures repository-relative for System.arg(1)."""
    case = Path(case)
    return case.relative_to(ROOT) if case.is_relative_to(ROOT) else case


def run_suite(nulang_bin: str, behavior_dir: Path = BEHAVIOR) -> tuple[int, int]:
    passed = failed = 0
    with tempfile.TemporaryDirectory(prefix="nulang-conformance-store-") as tmp:
        root = Path(tmp)
        for case in sorted(behavior_dir.glob("*.nula")):
            expectation = case.with_suffix(".json")
            if not expectation.exists():
                continue
            expected = json.loads(expectation.read_text(encoding="utf-8"))
            if run_case(nulang_bin, script_argument(case), expected, root):
                passed += 1
            else:
                failed += 1
    print(f"\n{passed} passed, {failed} failed.")
    return passed, failed


def main() -> int:
    subprocess.run(["cargo", "build", "--quiet", "--locked"], check=True, cwd=ROOT)
    # Respect custom target-dir in the repository's .cargo/config.toml.
    target_dir = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
    ))["target_directory"]
    _passed, failed = run_suite(os.path.join(target_dir, "debug", "nulang"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
