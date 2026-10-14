#!/usr/bin/env python3
"""Exercise the public Nulang CLI first-run path in a disposable workspace.

Use a previously built binary: python3 scripts/smoke_first_run.py --bin target/debug/nulang
This test never publishes, deploys, downloads packages, or modifies the repo.
"""

import argparse
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]


class SmokeFailure(Exception):
    pass


def check(condition: bool, message: str) -> None:
    if not condition:
        raise SmokeFailure(message)


def execute(binary: pathlib.Path, workdir: pathlib.Path, *args: str) -> str:
    label = " ".join(args)
    try:
        proc = subprocess.run(
            [str(binary), *args], cwd=workdir, capture_output=True,
            text=True, timeout=90, check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise SmokeFailure(f"{label}: timed out after 90 seconds") from error
    except OSError as error:
        raise SmokeFailure(f"{label}: could not launch: {error}") from error
    if proc.returncode != 0:
        raise SmokeFailure(
            f"{label}: exit {proc.returncode}\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    print(f"ok: {label}")
    return proc.stdout + proc.stderr


def smoke(binary: pathlib.Path) -> None:
    check(binary.is_file(), f"binary not found: {binary}. Build with cargo build --locked --bin nulang")
    execute(binary, ROOT, "--version")
    out = execute(binary, ROOT, "--eval", 'perform IO.print("first-run-eval-ok")')
    check("first-run-eval-ok" in out, "--eval did not print the expected greeting")

    with tempfile.TemporaryDirectory(prefix="nulang-first-run-") as tmp:
        workspace = pathlib.Path(tmp)
        execute(binary, workspace, "nula", "new", "smoke-app", "--template", "lib")
        package = workspace / "smoke-app"
        for relative in ("Nulang.toml", "src/main.nula", "tests/test_add.nula"):
            check((package / relative).is_file(), f"nula new omitted {relative}")

        execute(binary, package, "--check", "src/main.nula")
        execute(binary, package, "nula", "build")
        check((package / ".nula/dist/smoke-app.nbc").is_file(),
              "nula build did not produce the expected bytecode artifact")
        output = execute(binary, package, "nula", "run")
        check("Library package ready." in output, "nula run did not execute the template")
        result = execute(binary, package, "nula", "test")
        check("passed" in result.lower(), "nula test did not report passing tests")

        # Restrict this formatter smoke to constructs currently implemented by
        # src/fmt.rs. Formatting the more complex scaffold is not yet supported.
        format_file = package / "stripped.nula"
        format_file.write_text("type SmokeColor=Red|Green\n", encoding="utf-8")
        execute(binary, package, "fmt", "stripped.nula")
        formatted = format_file.read_text(encoding="utf-8")
        check(formatted != "type SmokeColor=Red|Green\n", "fmt did not normalize the fixture")
        execute(binary, package, "fmt", "--check", "stripped.nula")
        check(format_file.read_text(encoding="utf-8") == formatted,
              "fmt --check changed the file")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", default="target/debug/nulang",
                        help="Path to an already-built Nulang executable")
    args = parser.parse_args()
    binary = pathlib.Path(args.bin).resolve()
    try:
        smoke(binary)
    except SmokeFailure as error:
        print(f"first-run smoke failed: {error}", file=sys.stderr)
        return 1
    print("first-run smoke passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
