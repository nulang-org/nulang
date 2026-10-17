#!/usr/bin/env python3
"""Deterministic report for externally verified Nulang coding-agent attempts.

This script never executes an agent, runs a compiler, or asserts that submitted
compiler/test outcomes are authentic. A trusted evaluator must produce JSONL.
"""

import argparse
import hashlib
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path


class InvalidBenchmark(ValueError):
    pass


def integer(value, field, minimum=0):
    if type(value) is not int or value < minimum:
        raise InvalidBenchmark(f"{field} must be an integer >= {minimum}")
    return value


def nonempty_string(value, field):
    if not isinstance(value, str) or not value.strip() or len(value) > 256:
        raise InvalidBenchmark(f"{field} must be a nonempty string (max 256 chars)")
    return value


def load_manifest(path):
    try:
        raw = path.read_bytes()
        manifest = json.loads(raw)
    except (OSError, ValueError) as exc:
        raise InvalidBenchmark(f"cannot read task manifest: {exc}") from exc
    if not isinstance(manifest, dict) or type(manifest.get("schema_version")) is not int or manifest["schema_version"] != 1:
        raise InvalidBenchmark("task manifest requires schema_version 1")
    suite_id = nonempty_string(manifest.get("suite_id"), "suite_id")
    trials = integer(manifest.get("trials"), "trials", 1)
    if trials > 100:
        raise InvalidBenchmark("trials must be <= 100")
    tasks = manifest.get("tasks")
    if not isinstance(tasks, list) or not tasks:
        raise InvalidBenchmark("task manifest must contain nonempty tasks list")
    identifiers = []
    for task in tasks:
        if not isinstance(task, dict):
            raise InvalidBenchmark("each task must be an object")
        identifiers.append(nonempty_string(task.get("id"), "task id"))
    if len(set(identifiers)) != len(identifiers):
        raise InvalidBenchmark("duplicate task identifiers")
    return suite_id, trials, set(identifiers), hashlib.sha256(raw).hexdigest()


def load_results(path, tasks, trials):
    rows = []
    seen = set()
    try:
        with path.open(encoding="utf-8") as handle:
            for number, line in enumerate(handle, 1):
                if not line.strip():
                    continue
                try:
                    row = json.loads(line)
                except ValueError as exc:
                    raise InvalidBenchmark(f"invalid JSON on line {number}: {exc}") from exc
                if not isinstance(row, dict):
                    raise InvalidBenchmark(f"line {number} must contain an object")
                model = nonempty_string(row.get("model"), "model")
                task = nonempty_string(row.get("task_id"), "task_id")
                if task not in tasks:
                    raise InvalidBenchmark(f"line {number}: unknown task {task}")
                trial = integer(row.get("trial"), "trial", 1)
                if trial > trials:
                    raise InvalidBenchmark(f"line {number}: trial exceeds manifest trials")
                key = (model, task, trial)
                if key in seen:
                    raise InvalidBenchmark(f"line {number}: duplicate model/task/trial")
                seen.add(key)
                for name in ("compiler_ok", "tests_ok"):
                    if type(row.get(name)) is not bool:
                        raise InvalidBenchmark(f"line {number}: {name} must be boolean")
                for name in ("repair_cycles", "input_tokens", "output_tokens", "elapsed_ms"):
                    integer(row.get(name), name)
                rows.append(row)
    except OSError as exc:
        raise InvalidBenchmark(f"cannot read result records: {exc}") from exc
    if not rows:
        raise InvalidBenchmark("no results; cannot compute benchmark scores")
    return rows


def summarize(suite, trials, tasks, rows, manifest_sha256):
    by_model = defaultdict(list)
    for row in rows:
        by_model[row["model"]].append(row)
    total_expected = len(tasks) * trials
    models = []
    for model, model_rows in sorted(by_model.items()):
        completed = len(model_rows)
        successes = [
            r for r in model_rows if r["compiler_ok"] and r["tests_ok"]
        ]
        token_total = sum(r["input_tokens"] + r["output_tokens"] for r in model_rows)
        success_count = len(successes)
        models.append({
            "model": model,
            "expected": total_expected,
            "completed": completed,
            "missing": total_expected - completed,
            "coverage": completed / total_expected,
            "successes": success_count,
            "success_rate_observed": success_count / completed,
            # Missing trials count as failures, avoiding inflated scores when
            # systems selectively submit easy tasks or successful attempts.
            "success_rate_all_tasks": success_count / total_expected,
            "total_tokens": token_total,
            "tokens_per_success": token_total / success_count if success_count else None,
            "median_repair_cycles_success": (
                statistics.median(r["repair_cycles"] for r in successes)
                if successes else None
            ),
            "median_elapsed_ms_success": (
                statistics.median(r["elapsed_ms"] for r in successes)
                if successes else None
            ),
        })
    return {
        "schema_version": 1,
        "suite_id": suite,
        "manifest_sha256": manifest_sha256,
        "task_count": len(tasks),
        "trials": trials,
        "verification": "self-reported; external evaluator must attest outcomes",
        "models": models,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tasks", type=Path, required=True)
    parser.add_argument("--results", type=Path, required=True)
    arguments = parser.parse_args(argv)
    try:
        suite, trials, tasks, manifest_sha256 = load_manifest(arguments.tasks)
        rows = load_results(arguments.results, tasks, trials)
        report = summarize(suite, trials, tasks, rows, manifest_sha256)
    except InvalidBenchmark as exc:
        print(f"agent coding benchmark: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(report, sort_keys=True, separators=(",", ":"), allow_nan=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
