#!/usr/bin/env python3
"""Reproducible *size* smoke for one matched Nulang/C/Go stdout workload.

This does not claim performance parity or a size winner across different
linking models. Records PT_INTERP and DT_NEEDED to make such mismatches visible.
"""

from __future__ import annotations

import argparse
import json
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

MESSAGE = "Hello from Nulang"
EXPECTED = (MESSAGE + "\n").encode("utf-8")


def run(command: list[str], *, cwd: Path | None = None) -> subprocess.CompletedProcess[bytes]:
    completed = subprocess.run(command, cwd=cwd, capture_output=True, timeout=120)
    if completed.returncode:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {command!r}\n"
            f"stderr: {completed.stderr.decode(errors='replace')[-2000:]}"
        )
    return completed


def elf_dependencies(binary: Path) -> dict[str, object]:
    """Read ELF metadata without executing the artifact or invoking ldd."""
    if shutil.which("readelf") is None:
        return {"pt_interp": None, "needed_libraries": None}
    program_headers = run(["readelf", "-l", str(binary)]).stdout.decode(errors="replace")
    dynamic_section = run(["readelf", "-d", str(binary)]).stdout.decode(errors="replace")
    return {
        "pt_interp": bool(re.search(r"^\s*INTERP\s", program_headers, re.MULTILINE)),
        "needed_libraries": re.findall(r"\(NEEDED\).*?\[(.*?)\]", dynamic_section),
    }


def inspect(name: str, path: Path) -> dict[str, object]:
    if path.read_bytes()[:4] != b"\x7fELF":
        raise RuntimeError(f"{name} is not an ELF executable: {path}")
    output = run([str(path)]).stdout
    if output != EXPECTED:
        raise RuntimeError(f"{name} output mismatch: {output!r} != {EXPECTED!r}")
    return {"name": name, "bytes": path.stat().st_size, **elf_dependencies(path)}


def measure(nulang: Path | None) -> dict[str, object]:
    if shutil.which("cc") is None:
        raise RuntimeError("C compiler/linker 'cc' is required")
    if nulang is not None and not nulang.is_file():
        raise RuntimeError(f"Nulang compiler not found: {nulang}")

    with tempfile.TemporaryDirectory(prefix="nulang-size-") as tmp:
        directory = Path(tmp)
        c_source = directory / "main.c"
        c_source.write_text(
            f'#include <stdio.h>\nint main(void) {{ puts("{MESSAGE}"); return 0; }}\n'
        )
        c_binary = directory / "c"
        run(["cc", "-Os", "-s", "-Wl,--gc-sections", str(c_source), "-o", str(c_binary)])

        results = [inspect("C libc (-Os -s)", c_binary)]
        if nulang is not None:
            source = directory / "hello.nula"
            source.write_text(f'perform IO.print("{MESSAGE}")\n')
            native = directory / "nulang"
            run([str(nulang.resolve()), "--emit-exe", "--out", str(native), str(source)])
            results.append(inspect("Nulang experimental libc", native))

        if shutil.which("go"):
            go_source = directory / "main.go"
            go_source.write_text(
                f'package main\nimport "fmt"\nfunc main() {{ fmt.Println("{MESSAGE}") }}\n'
            )
            go_binary = directory / "go"
            run([
                "go", "build", "-trimpath", "-ldflags=-s -w -buildid=",
                "-o", str(go_binary), str(go_source),
            ])
            results.append(inspect("Go fmt.Println (-s -w)", go_binary))

        return {
            "schema_version": 1,
            "platform": {"system": platform.system(), "architecture": platform.machine()},
            "toolchains": {
                "cc": run(["cc", "--version"]).stdout.decode(errors="replace").splitlines()[0],
                "go": run(["go", "version"]).stdout.decode(errors="replace").strip() if shutil.which("go") else None,
                "nulang": str(nulang.resolve()) if nulang is not None else None,
            },
            "workload": "one_literal_stdout_line",
            "expected_stdout": EXPECTED.decode(),
            "comparability": (
                "Artifacts use their toolchains' default runtime/linkage models. "
                "Compare PT_INTERP and DT_NEEDED before interpreting byte counts; "
                "this is not a cross-language performance benchmark."
            ),
            "artifacts": results,
        }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--nulang", type=Path, help="path to built Nulang compiler")
    group.add_argument("--baseline-only", action="store_true", help="C/Go baselines only")
    parser.add_argument("--json-out", type=Path, help="optional JSON report destination")
    opts = parser.parse_args()
    try:
        report = measure(opts.nulang)
    except (RuntimeError, subprocess.TimeoutExpired, OSError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    encoded = json.dumps(report, indent=2, ensure_ascii=False)
    if opts.json_out:
        opts.json_out.write_text(encoded + "\n")
    print(encoded)
    return 0


if __name__ == "__main__":
    sys.exit(main())
