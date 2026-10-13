#!/usr/bin/env python3
"""Stable verification profiles for Nulang contributors and coding agents."""
from __future__ import annotations

import argparse
import pathlib
import shlex
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]

PROFILES = {
    "fast": (
        ["python3", "scripts/verify_semantics.py"],
        ["cargo", "test", "--locked", "difffuzz"],
    ),
    "native": (
        ["python3", "scripts/verify_semantics.py"],
        [
            "cargo",
            "test",
            "--locked",
            "--no-default-features",
            "--features",
            "native-codegen",
            "difffuzz",
        ],
        [
            "cargo",
            "test",
            "--locked",
            "--no-default-features",
            "--features",
            "native-codegen",
            "aot",
        ],
    ),
    "wasm": (
        ["python3", "scripts/verify_semantics.py"],
        ["cargo", "test", "--locked", "--features", "wasm-backend", "difffuzz"],
    ),
    "durability": (
        ["python3", "scripts/verify_semantics.py"],
        ["cargo", "test", "--locked", "--test", "durable_effect_recovery_store"],
        ["cargo", "test", "--locked", "commit_transition"],
    ),
    "full": (
        ["python3", "scripts/verify_semantics.py"],
        ["bash", "scripts/ci-local.sh"],
    ),
}


def profile_commands(profile: str) -> list[list[str]]:
    try:
        return [list(command) for command in PROFILES[profile]]
    except KeyError as exc:
        raise ValueError(f"unknown verification profile: {profile}") from exc


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", choices=tuple(PROFILES))
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the commands for the profile without executing them.",
    )
    args = parser.parse_args(argv)

    for command in profile_commands(args.profile):
        print(f"==> {shlex.join(command)}")
        if args.dry_run:
            continue
        completed = subprocess.run(command, cwd=ROOT, check=False)
        if completed.returncode != 0:
            return completed.returncode

    print(f"verification profile '{args.profile}' passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
