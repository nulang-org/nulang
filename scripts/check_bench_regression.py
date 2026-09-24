#!/usr/bin/env python3
"""Regression-gate benchmark results against a rolling window of recent
main-branch history.

Reads the latest collected result (schema from `collect_bench_results.py`:
one JSON object per line, `{benchmark, mean_ns, mean_ns_lower,
mean_ns_upper}`) and compares each benchmark's `mean_ns` against a rolling
window of that same benchmark's history in `benchmarks/`.

This is deliberately NOT a flat percentage cutoff. `benchmarks/README.md`
documents why: GitHub Actions' shared runners commonly show 20-50%+
run-to-run noise on wall-clock-sensitive benchmarks (neighbor CPU/cache
contention), so a naive fixed threshold fails PRs for noise, not real
regressions. Instead, each benchmark's own historical spread sets its
threshold:

    threshold = max(MIN_PCT, K * (MAD / median))

`MAD` (median absolute deviation) is a robust, outlier-resistant spread
estimate computed from that benchmark's own rolling window -- a
consistently-quiet benchmark gets a tight threshold, a naturally-noisy one
gets a wide one, automatically. `MIN_PCT` is a floor so a benchmark that
happened to be unusually stable across its last few samples doesn't get
flagged for a trivial, practically-meaningless delta.

A benchmark is only gated once it has at least `--min-samples` prior
results in the rolling window; new or rarely-run benchmarks are reported
as skipped, not failed.

Usage:
    check_bench_regression.py --latest FILE --history-dir DIR
"""
import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path


def load_results(path: Path) -> dict[str, float]:
    """Load one JSON-lines result file into {benchmark: mean_ns}."""
    results = {}
    with path.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            entry = json.loads(line)
            results[entry["benchmark"]] = entry["mean_ns"]
    return results


def median_absolute_deviation(samples: list[float], center: float) -> float:
    """MAD: median of absolute deviations from `center`. Robust to
    outliers, unlike standard deviation -- a single bad-neighbor-CPU run
    in the window doesn't blow out the spread estimate."""
    deviations = [abs(s - center) for s in samples]
    return statistics.median(deviations)


def git_commit_times() -> dict[str, int]:
    """Return commit timestamps keyed by full SHA for the checked-out history.

    Benchmark result filenames are the main-branch commit SHA that produced
    them. Use Git history rather than filesystem mtimes: history JSON is
    materialized with `git show` during CI, so its mtime describes copy order,
    not benchmark chronology.
    """
    try:
        proc = subprocess.run(
            ["git", "log", "--format=%H %ct"],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError:
        return {}

    if proc.returncode != 0:
        return {}

    commit_times: dict[str, int] = {}
    for line in proc.stdout.splitlines():
        sha, separator, timestamp = line.partition(" ")
        if not separator:
            continue
        try:
            commit_times[sha] = int(timestamp)
        except ValueError:
            continue
    return commit_times


def history_order_key(path: Path, commit_times: dict[str, int]) -> tuple[int, int, str]:
    """Sort benchmark files newest-first by producing commit timestamp.

    Non-SHA or unavailable commits retain a deterministic mtime fallback for
    local/offline use, but CI checks out full history so normal benchmark files
    always take the Git-timestamp path.
    """
    timestamp = commit_times.get(path.stem)
    if timestamp is not None:
        return (1, timestamp, path.name)
    return (0, path.stat().st_mtime_ns, path.name)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--latest", required=True, type=Path, help="just-collected result JSON-lines file"
    )
    parser.add_argument(
        "--history-dir", required=True, type=Path, help="directory of prior <sha>.json result files"
    )
    parser.add_argument(
        "--window", type=int, default=10, help="max prior commits to compare against (default: 10)"
    )
    parser.add_argument(
        "--min-samples",
        type=int,
        default=3,
        help="minimum prior samples required before gating a benchmark (default: 3)",
    )
    parser.add_argument(
        "--mad-multiplier",
        type=float,
        default=6.0,
        help="how many MADs beyond the rolling median counts as a regression (default: 6.0, "
        "conservative -- roughly 4 robust-equivalent standard deviations)",
    )
    parser.add_argument(
        "--min-pct",
        type=float,
        default=0.20,
        help="floor on the regression threshold as a fraction of the median, so a benchmark "
        "with unusually low historical spread isn't flagged for a trivial delta (default: 0.20)",
    )
    args = parser.parse_args()

    latest = load_results(args.latest)
    if not latest:
        print("No benchmarks in the latest result file; nothing to check.")
        return 0

    latest_resolved = args.latest.resolve()
    commit_times = git_commit_times()
    history_candidates = [
        p
        for p in args.history_dir.glob("*.json")
        if p.resolve() != latest_resolved
    ]
    history_files = sorted(
        history_candidates,
        key=lambda p: history_order_key(p, commit_times),
        reverse=True,
    )[: args.window]

    unresolved = [
        path.name
        for path in history_files
        if path.stem not in commit_times
    ]
    if unresolved:
        print(
            "WARNING: Git timestamps unavailable for "
            f"{len(unresolved)} selected history file(s); using filesystem mtime fallback: "
            + ", ".join(unresolved)
        )

    if len(history_files) < args.min_samples:
        print(
            f"Only {len(history_files)} prior result file(s) in {args.history_dir} "
            f"(need >= {args.min_samples}); skipping regression gate -- not enough "
            "history to compare against yet."
        )
        return 0

    # benchmark -> list of historical mean_ns values, most recent window only.
    history: dict[str, list[float]] = {}
    for path in history_files:
        for name, mean_ns in load_results(path).items():
            history.setdefault(name, []).append(mean_ns)

    regressions = []
    skipped = []
    for name, current_ns in sorted(latest.items()):
        samples = history.get(name, [])
        if len(samples) < args.min_samples:
            skipped.append((name, len(samples)))
            continue
        baseline = statistics.median(samples)
        if baseline <= 0:
            continue
        mad = median_absolute_deviation(samples, baseline)
        mad_ratio = mad / baseline
        threshold = max(args.min_pct, args.mad_multiplier * mad_ratio)
        delta = (current_ns - baseline) / baseline
        if delta > threshold:
            regressions.append((name, baseline, current_ns, delta, threshold))

    if skipped:
        print(f"Skipped {len(skipped)} benchmark(s) with < {args.min_samples} historical samples:")
        for name, n in skipped:
            print(f"  - {name} ({n} prior sample(s))")

    if not regressions:
        print(
            f"OK: no benchmark regressed beyond its own noise-adjusted threshold "
            f"(rolling window: {len(history_files)} commit(s))."
        )
        return 0

    print(
        f"REGRESSION: {len(regressions)} benchmark(s) exceeded their noise-adjusted "
        f"threshold (rolling window: {len(history_files)} commit(s)):"
    )
    for name, baseline, current_ns, delta, threshold in sorted(regressions, key=lambda r: -r[3]):
        print(
            f"  - {name}: {baseline:,.0f}ns (rolling median) -> {current_ns:,.0f}ns "
            f"({delta:+.1%}, threshold was {threshold:.1%})"
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
