#!/usr/bin/env python3
"""Paired warm import-compilation timing for one Nulang binary.

Alternating runs control order effects; each run uses a fresh process and its
own warmup followed by repeated compiles. Does not build the Rust binary.
"""

import argparse
import json
import os
import pathlib
import statistics
import subprocess


def parse_samples(output: str, mode: str, repeat: int, modules: int, functions: int) -> float:
    try:
        rows = [json.loads(line) for line in output.splitlines() if line.strip()]
    except json.JSONDecodeError as exc:
        raise ValueError(f"invalid JSONL: {exc}") from exc
    if len(rows) != repeat:
        raise ValueError(f"expected {repeat} samples for {mode}, received {len(rows)}")
    samples = []
    for index, row in enumerate(rows, start=1):
        if not isinstance(row, dict):
            raise ValueError("each sample must be a JSON object")
        expected = {
            "schema": 1,
            "workload": "transitive_imports",
            "cache": mode,
            "scope": "in_process_import_to_bytecode",
            "iteration": index,
            "modules": modules,
            "functions_per_module": functions,
            "resolved_functions": modules * functions,
        }
        for key, value in expected.items():
            if row.get(key) != value:
                raise ValueError(f"invalid {mode} sample {index}: {key} must be {value!r}")
        duration = row.get("compile_ns")
        if isinstance(duration, bool) or not isinstance(duration, int) or duration <= 0:
            raise ValueError(f"invalid {mode} sample {index}: compile_ns must be positive integer")
        samples.append(duration)
    return statistics.median(samples)


def run_mode(binary: pathlib.Path, mode: str, repeat: int, warmup: int, modules: int, functions: int) -> float:
    env = os.environ.copy()
    if mode == "disabled":
        env["NULANG_DISABLE_IMPORT_TOKEN_CACHE"] = "1"
    else:
        env.pop("NULANG_DISABLE_IMPORT_TOKEN_CACHE", None)
    command = [
        str(binary), "--format", "jsonl", "--repeat", str(repeat),
        "--warmup", str(warmup), "--modules", str(modules),
        "--functions", str(functions),
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False, env=env)
    if result.returncode:
        raise RuntimeError(f"{mode} benchmark failed ({result.returncode}): {result.stderr.strip()}")
    return parse_samples(result.stdout, mode, repeat, modules, functions)


def paired_comparison(binary: pathlib.Path, pairs: int, repeat: int, warmup: int, modules: int, functions: int) -> dict:
    samples = []
    for pair in range(pairs):
        modes = ("enabled", "disabled") if pair % 2 == 0 else ("disabled", "enabled")
        values = {}
        for mode in modes:
            values[mode] = run_mode(binary, mode, repeat, warmup, modules, functions)
        samples.append({
            "pair": pair + 1,
            "enabled_median_ns": values["enabled"],
            "disabled_median_ns": values["disabled"],
            "speedup_disabled_over_enabled": values["disabled"] / values["enabled"],
        })
    return {
        "schema": 1,
        "binary": str(binary),
        "pairs": pairs,
        "repetitions_per_mode": repeat,
        "warmup_per_mode": warmup,
        "modules": modules,
        "functions_per_module": functions,
        "median_speedup_disabled_over_enabled": statistics.median(
            row["speedup_disabled_over_enabled"] for row in samples
        ),
        "samples": samples,
        "note": "Diagnostic only; no statistical significance or cross-language speed claim",
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, default=pathlib.Path("target/release/nulang-import-bench"))
    parser.add_argument("--pairs", type=int, default=4)
    parser.add_argument("--repeat", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--modules", type=int, default=8)
    parser.add_argument("--functions", type=int, default=16)
    args = parser.parse_args()
    if not (1 <= args.pairs <= 100 and 1 <= args.repeat <= 1000 and 0 <= args.warmup <= 1000):
        parser.error("pairs/repeat must be 1..100 or 1..1000; warmup must be 0..1000")
    if not (1 <= args.modules <= 32 and 1 <= args.functions <= 64):
        parser.error("modules must be 1..32, functions 1..64")
    if not args.binary.is_file():
        parser.error(f"benchmark binary does not exist: {args.binary}")
    print(json.dumps(paired_comparison(args.binary.resolve(), args.pairs, args.repeat, args.warmup, args.modules, args.functions), indent=2))


if __name__ == "__main__":
    main()
