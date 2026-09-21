#!/usr/bin/env python3
"""Render a same-host Nulang-vs-BEAM actor benchmark comparison."""

from __future__ import annotations

import argparse
import csv
from pathlib import Path


def load(path: Path) -> dict[tuple[str, int], dict[str, int | str]]:
    rows: dict[tuple[str, int], dict[str, int | str]] = {}
    with path.open(newline="") as handle:
        for row in csv.DictReader(handle):
            key = (row["benchmark"], int(row["operations"]))
            rows[key] = {
                "runtime": row["runtime"],
                "elapsed_us": int(row["elapsed_us"]),
                "ops_per_sec": int(row["ops_per_sec"]),
            }
    return rows


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--nulang", type=Path, required=True)
    parser.add_argument("--beam", type=Path, required=True)
    args = parser.parse_args()

    nulang = load(args.nulang)
    beam = load(args.beam)
    keys = sorted(set(nulang) & set(beam), key=lambda item: (item[1], item[0]))

    if not keys:
        raise SystemExit("no matching Nulang/BEAM benchmark rows")

    print("## Same-host Nulang vs BEAM actor baseline")
    print()
    print("| Benchmark | Ops | Nulang ops/s | BEAM ops/s | Nulang / BEAM |")
    print("|---|---:|---:|---:|---:|")
    for benchmark, operations in keys:
        n = int(nulang[(benchmark, operations)]["ops_per_sec"])
        b = int(beam[(benchmark, operations)]["ops_per_sec"])
        ratio = n / b if b else float("inf")
        print(
            f"| {benchmark} | {operations:,} | {n:,} | {b:,} | {ratio:.2f}× |"
        )

    missing_nulang = sorted(set(beam) - set(nulang))
    missing_beam = sorted(set(nulang) - set(beam))
    if missing_nulang or missing_beam:
        print()
        print("Unmatched rows are excluded from the table.")
        if missing_nulang:
            print(f"- Missing from Nulang: {missing_nulang}")
        if missing_beam:
            print(f"- Missing from BEAM: {missing_beam}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
