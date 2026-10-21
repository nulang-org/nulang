#!/usr/bin/env python3
"""Validate a packaged Nulang release archive, then smoke-test the extracted CLI.

Runs using only Python's standard library; no checkout/build dependencies are
required. A cross-compiled archive can be checked with --no-execute, but a
separate native runner must execute that target before publication.
"""
import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile


class VerificationError(Exception):
    """A release archive did not satisfy its publication contract."""


def verify_artifact(
    archive: Path,
    checksum: Path,
    member: str,
    tag: str,
    *,
    execute: bool = True,
) -> None:
    archive, checksum = Path(archive), Path(checksum)
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?", tag):
        raise VerificationError(f"invalid version tag: {tag!r}")
    if not re.fullmatch(r"nulang-[A-Za-z0-9_-]+", member):
        raise VerificationError(f"invalid expected binary name: {member!r}")
    if not archive.is_file() or not checksum.is_file():
        raise VerificationError("release archive or checksum file is missing")

    lines = checksum.read_text(encoding="utf-8").strip().splitlines()
    if len(lines) != 1:
        raise VerificationError("checksum file must contain exactly one entry")
    match = re.fullmatch(r"([0-9a-fA-F]{64})\s+\*?(.+)", lines[0])
    if match is None:
        raise VerificationError("invalid SHA-256 checksum line")
    digest, filename = match.groups()
    if filename != archive.name:
        raise VerificationError(
            f"checksum filename mismatch: expected {archive.name!r}, got {filename!r}"
        )
    with archive.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual.lower() != digest.lower():
        raise VerificationError("release archive checksum mismatch")

    with tempfile.TemporaryDirectory(prefix="nulang-release-smoke-") as tmp:
        executable = Path(tmp) / (member + (".exe" if os.name == "nt" else ""))
        try:
            with tarfile.open(archive, mode="r:gz") as packed:
                members = packed.getmembers()
                if len(members) != 1:
                    raise VerificationError(
                        f"archive must contain exactly one file, found {len(members)}"
                    )
                item = members[0]
                if not item.isfile() or item.name != member:
                    raise VerificationError(
                        f"archive does not contain expected binary {member!r}"
                    )
                payload = packed.extractfile(item)
                if payload is None:
                    raise VerificationError("expected binary could not be read")
                with payload, executable.open("wb") as out:
                    while chunk := payload.read(1024 * 1024):
                        out.write(chunk)
        except (tarfile.TarError, OSError) as exc:
            raise VerificationError(f"invalid release archive: {exc}") from exc

        if not execute:
            print(f"Archive and checksum verified (execution deferred): {archive.name}")
            return

        if os.name != "nt":
            executable.chmod(0o755)
        version = tag[1:]
        for args, expected in (
            (["--version"], version),
            (["--eval", 'perform IO.print("release smoke ok")'], "release smoke ok"),
        ):
            try:
                result = subprocess.run(
                    [str(executable), *args],
                    text=True,
                    capture_output=True,
                    timeout=30,
                    check=False,
                )
            except (OSError, subprocess.TimeoutExpired) as exc:
                raise VerificationError(
                    f"packaged CLI could not run ({' '.join(args)}): {exc}"
                ) from exc
            if args[0] == "--version":
                # A substring would accept the wrong binary, e.g. version
                # 0.1.0 appearing inside 10.1.0 or 0.1.0-debug.
                # The CLI may print a second "language ..." metadata line.
                version_lines = [line.strip().split() for line in result.stdout.splitlines()]
                output_valid = any(
                    len(fields) == 2
                    and fields[0].casefold() == "nulang"
                    and fields[1] == version
                    for fields in version_lines
                )
            else:
                output_valid = expected in result.stdout
            if result.returncode != 0 or not output_valid:
                raise VerificationError(
                    f"packaged CLI failed {' '.join(args)}: "
                    f"exit={result.returncode}; stdout={result.stdout[:400]!r}; "
                    f"stderr={result.stderr[:400]!r}; expected {expected!r}"
                )
        print(f"Packaged CLI passed native smoke checks: {archive.name} ({tag})")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--checksum", type=Path, required=True)
    parser.add_argument("--member", required=True, help="expected tar member name")
    parser.add_argument("--tag", required=True, help="Git tag, e.g. v0.1.0")
    parser.add_argument("--no-execute", action="store_true")
    args = parser.parse_args()
    try:
        verify_artifact(
            args.archive, args.checksum, args.member, args.tag,
            execute=not args.no_execute,
        )
    except VerificationError as exc:
        print(f"Release verification failed: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
