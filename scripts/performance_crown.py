#!/usr/bin/env python3
"""Validate and summarize Nulang's performance-coverage contract.

The manifest separates existing microbenchmarks from end-to-end measurements,
controlled comparative benchmarks, and explicit gaps. It is deliberately a
coverage contract, not a leaderboard: comparative readiness says a workload is
suitable for collecting cross-runtime evidence, not that Nulang wins it.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "benchmarks" / "performance_crown.json"
REQUIRED_DOMAINS = ("local", "concurrent", "distributed", "durable", "application")
ALLOWED_STATUS = {"microbenchmark", "end_to_end", "comparative", "planned"}


def rows(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    flattened: list[dict[str, Any]] = []
    for domain in manifest.get("domains", []):
        domain_id = domain.get("id")
        for benchmark in domain.get("benchmarks", []):
            baselines = benchmark.get("baselines") or []
            measurement_mode = benchmark.get("measurement_mode")
            status = benchmark.get("status")
            flattened.append(
                {
                    "domain": domain_id,
                    **benchmark,
                    "claim_ready": (
                        status == "comparative"
                        and measurement_mode == "controlled"
                        and bool(baselines)
                    ),
                }
            )
    return flattened


def validate_manifest(
    manifest: dict[str, Any], root: Path | None = None
) -> list[str]:
    errors: list[str] = []
    if manifest.get("schema") != 1:
        errors.append("schema must be 1")

    domains = manifest.get("domains")
    if not isinstance(domains, list):
        return errors + ["domains must be a list"]

    domain_ids = [domain.get("id") for domain in domains if isinstance(domain, dict)]
    missing = sorted(set(REQUIRED_DOMAINS) - set(domain_ids))
    extra = sorted(set(domain_ids) - set(REQUIRED_DOMAINS))
    if missing:
        errors.append(f"missing required domains: {', '.join(missing)}")
    if extra:
        errors.append(f"unknown domains: {', '.join(extra)}")
    if len(domain_ids) != len(set(domain_ids)):
        errors.append("domain ids must be unique")

    seen_benchmarks: set[str] = set()
    for domain in domains:
        if not isinstance(domain, dict):
            errors.append("each domain must be an object")
            continue
        domain_id = domain.get("id")
        benchmarks = domain.get("benchmarks")
        if not isinstance(benchmarks, list) or not benchmarks:
            errors.append(f"{domain_id}: benchmarks must be a non-empty list")
            continue

        for benchmark in benchmarks:
            if not isinstance(benchmark, dict):
                errors.append(f"{domain_id}: benchmark entries must be objects")
                continue
            benchmark_id = benchmark.get("id")
            label = benchmark_id or f"{domain_id}:<unnamed>"
            if not isinstance(benchmark_id, str) or not benchmark_id.startswith(f"{domain_id}."):
                errors.append(f"{label}: id must start with {domain_id}.")
            elif benchmark_id in seen_benchmarks:
                errors.append(f"{benchmark_id}: benchmark id must be unique")
            else:
                seen_benchmarks.add(benchmark_id)

            status = benchmark.get("status")
            if status not in ALLOWED_STATUS:
                errors.append(f"{label}: status must be one of {sorted(ALLOWED_STATUS)}")

            metric = benchmark.get("metric")
            if not isinstance(metric, str) or not metric.strip():
                errors.append(f"{label}: metric is required")

            runner = benchmark.get("runner")
            if status != "planned" and not runner:
                errors.append(f"{label}: non-planned benchmark requires runner")
            if runner:
                runner_path = Path(runner)
                if runner_path.is_absolute() or ".." in runner_path.parts:
                    errors.append(f"{label}: runner must be a repository-relative path")
                elif root is not None and not (root / runner_path).is_file():
                    errors.append(f"{label}: runner does not exist: {runner}")

            baselines = benchmark.get("baselines")
            if not isinstance(baselines, list):
                errors.append(f"{label}: baselines must be a list")
                baselines = []
            if "nulang" in baselines:
                errors.append(f"{label}: baselines must not include nulang itself")

            if status == "comparative":
                if not baselines:
                    errors.append(f"{label}: comparative benchmark requires at least one baseline")
                if benchmark.get("measurement_mode") != "controlled":
                    errors.append(f"{label}: comparative benchmark requires measurement_mode=controlled")

    return errors


def load_manifest(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as handle:
        data = json.load(handle)
    if not isinstance(data, dict):
        raise ValueError("performance crown manifest must contain a JSON object")
    return data


def print_table(manifest: dict[str, Any]) -> None:
    print("domain       benchmark                                      status          evidence")
    print("-----------  ---------------------------------------------  --------------  --------")
    for row in rows(manifest):
        evidence = "comparative-ready" if row["claim_ready"] else "not-comparative"
        print(
            f"{row['domain']:<11}  {row['id']:<45}  {row['status']:<14}  {evidence}"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--check", action="store_true", help="validate and exit")
    parser.add_argument("--json", action="store_true", help="emit flattened JSON rows")
    args = parser.parse_args()

    manifest = load_manifest(args.manifest)
    errors = validate_manifest(manifest, ROOT)
    if errors:
        for error in errors:
            print(f"error: {error}")
        return 1

    if args.check:
        print(f"performance crown manifest valid: {len(rows(manifest))} workloads")
    elif args.json:
        print(json.dumps(rows(manifest), indent=2, sort_keys=True))
    else:
        print_table(manifest)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
