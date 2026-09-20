#!/usr/bin/env python3
"""Run Nulang's informational cross-language performance suite.

Builds compiled implementations once, then measures process execution for
identical long-running workloads. Results are checksum-validated and include
host/toolchain metadata. This is intentionally separate from the Criterion
regression gate.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
SUITE_DIR = Path(__file__).resolve().parent
LANGUAGE_ORDER = ["nulang", "cpp", "rust", "go", "java", "node", "python", "erlang"]
EXTENSIONS = {
    "nulang": "nula",
    "cpp": "cpp",
    "rust": "rs",
    "go": "go",
    "java": "java",
    "node": "js",
    "python": "py",
    "erlang": "erl",
}


def run_checked(
    cmd: list[str],
    *,
    cwd: Path | None = None,
    timeout: float = 120.0,
    env_overrides: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    if env_overrides:
        env.update(env_overrides)
    result = subprocess.run(
        cmd,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=timeout,
        check=False,
        env=env,
    )
    if result.returncode != 0:
        rendered = " ".join(cmd)
        raise RuntimeError(
            f"command failed ({result.returncode}): {rendered}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def first_line(cmd: list[str]) -> str | None:
    try:
        result = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    text = (result.stdout + "\n" + result.stderr).strip()
    return text.splitlines()[0] if text else None


def cpu_model() -> str:
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.lower().startswith("model name"):
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
    return platform.processor() or "unknown"


def discover_nulang_bin(explicit: Path | None) -> Path:
    """Resolve the release CLI through Cargo's configured target directory."""
    if explicit is not None:
        return explicit.resolve()

    env_target = os.environ.get("CARGO_TARGET_DIR")
    if env_target:
        target_dir = Path(env_target)
    else:
        try:
            metadata = run_checked(
                ["cargo", "metadata", "--format-version", "1", "--no-deps"],
                cwd=ROOT,
                timeout=30.0,
            )
            target_dir = Path(json.loads(metadata.stdout)["target_directory"])
        except (OSError, RuntimeError, subprocess.SubprocessError, KeyError, json.JSONDecodeError):
            target_dir = ROOT / "target"

    executable = "nulang.exe" if os.name == "nt" else "nulang"
    return (target_dir / "release" / executable).resolve()


def toolchain_versions(nulang_bin: Path) -> dict[str, str | None]:
    return {
        "nulang": first_line([str(nulang_bin), "--version"]) if nulang_bin.exists() else None,
        "rust": first_line(["rustc", "--version"]) if shutil.which("rustc") else None,
        "cpp": first_line(["g++", "--version"]) if shutil.which("g++") else None,
        "go": first_line(["go", "version"]) if shutil.which("go") else None,
        "java": first_line(["java", "-version"]) if shutil.which("java") else None,
        "node": first_line(["node", "--version"]) if shutil.which("node") else None,
        "python": first_line([sys.executable, "--version"]),
        "erlang": first_line(["erl", "-noshell", "-eval", 'io:format("OTP ~s~n", [erlang:system_info(otp_release)]), halt().']) if shutil.which("erl") else None,
    }


def available(language: str, nulang_bin: Path) -> bool:
    if language == "nulang":
        return nulang_bin.exists()
    tool = {
        "cpp": "g++",
        "rust": "rustc",
        "go": "go",
        "java": "javac",
        "node": "node",
        "python": None,
        "erlang": "erlc",
    }[language]
    return True if tool is None else shutil.which(tool) is not None


def build_command(language: str, workload_dir: Path, build_dir: Path, nulang_bin: Path) -> list[str]:
    source = workload_dir / ("bench.erl" if language == "erlang" else f"{language}.{EXTENSIONS[language]}")
    if language == "nulang":
        artifact = build_dir / "nulang.nbc"
        run_checked(
            [
                str(nulang_bin),
                "--backend",
                "bytecode",
                "--emit-nbc",
                "--out",
                str(artifact),
                str(source),
            ],
            cwd=ROOT,
        )
        return [str(nulang_bin), str(artifact)]
    if language == "cpp":
        out = build_dir / "cpp"
        run_checked(["g++", "-O3", "-march=native", "-DNDEBUG", "-std=c++20", str(source), "-o", str(out)])
        return [str(out)]
    if language == "rust":
        out = build_dir / "rust"
        run_checked([
            "rustc",
            "-O",
            "-C",
            "opt-level=3",
            "-C",
            "target-cpu=native",
            "-C",
            "codegen-units=1",
            str(source),
            "-o",
            str(out),
        ])
        return [str(out)]
    if language == "go":
        out = build_dir / "go"
        run_checked(["go", "build", "-o", str(out), str(source)])
        return [str(out)]
    if language == "java":
        classes = build_dir / "java"
        classes.mkdir(parents=True, exist_ok=True)
        run_checked(["javac", "-d", str(classes), str(source)])
        return ["java", "-cp", str(classes), "Main"]
    if language == "node":
        return ["node", str(source)]
    if language == "python":
        return [sys.executable, str(source)]
    if language == "erlang":
        run_checked(["erlc", "-o", str(build_dir), str(source)])
        return ["erl", "-noshell", "-pa", str(build_dir), "-s", "bench", "main", "-s", "init", "stop"]
    raise ValueError(f"unsupported language: {language}")


def checksum_present(stdout: str, expected: str) -> bool:
    return re.search(rf"(?<!\d){re.escape(expected)}(?!\d)", stdout) is not None


def sample_command(
    cmd: list[str],
    *,
    expected: str,
    warmups: int,
    runs: int,
    timeout: float,
    env_overrides: dict[str, str] | None = None,
) -> list[float]:
    for _ in range(warmups):
        result = run_checked(
            cmd, cwd=ROOT, timeout=timeout, env_overrides=env_overrides
        )
        if not checksum_present(result.stdout, expected):
            raise RuntimeError(
                f"checksum {expected} missing from output of {' '.join(cmd)}; "
                f"stdout={result.stdout!r}"
            )

    samples_ms: list[float] = []
    for _ in range(runs):
        start = time.perf_counter_ns()
        result = run_checked(
            cmd, cwd=ROOT, timeout=timeout, env_overrides=env_overrides
        )
        elapsed_ns = time.perf_counter_ns() - start
        if not checksum_present(result.stdout, expected):
            raise RuntimeError(
                f"checksum {expected} missing from output of {' '.join(cmd)}; "
                f"stdout={result.stdout!r}"
            )
        samples_ms.append(elapsed_ns / 1_000_000.0)
    return samples_ms


def measure_peak_rss_kb(
    cmd: list[str],
    *,
    expected: str,
    timeout: float,
    env_overrides: dict[str, str] | None = None,
) -> int | None:
    """Measure one checksum-validated process with GNU time, when available."""
    time_bin = Path("/usr/bin/time")
    if not time_bin.exists():
        return None

    with tempfile.NamedTemporaryFile(prefix="nulang-rss-", delete=False) as handle:
        rss_path = Path(handle.name)

    try:
        result = run_checked(
            [str(time_bin), "-f", "%M", "-o", str(rss_path), *cmd],
            cwd=ROOT,
            timeout=timeout,
            env_overrides=env_overrides,
        )
        if not checksum_present(result.stdout, expected):
            raise RuntimeError(
                f"checksum {expected} missing from RSS probe of {' '.join(cmd)}; "
                f"stdout={result.stdout!r}"
            )
        raw = rss_path.read_text().strip()
        return int(raw) if raw else None
    finally:
        try:
            rss_path.unlink()
        except OSError:
            pass


def summarize(samples: list[float]) -> dict[str, Any]:
    median_ms = statistics.median(samples)
    mad_ms = statistics.median(abs(sample - median_ms) for sample in samples)
    return {
        "samples_ms": samples,
        "median_ms": median_ms,
        "mad_ms": mad_ms,
        "min_ms": min(samples),
        "max_ms": max(samples),
    }


def markdown_report(report: dict[str, Any]) -> str:
    lines = [
        "# Nulang cross-language benchmark",
        "",
        f"Generated: {report['generated_at_utc']}",
        "",
        f"Host: {report['host']['cpu']} / {report['host']['machine']} / {report['host']['platform']}",
        "",
        "| Workload | Language | Median ms | MAD ms | Min ms | Max ms | M work units/s | Peak RSS MiB | Inc bytes/entity | Time ratio vs Nulang |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for workload in report["workloads"]:
        baseline = next(
            (r["median_ms"] for r in workload["results"] if r["language"] == "nulang"),
            None,
        )
        for result in workload["results"]:
            ratio = result["median_ms"] / baseline if baseline else None
            ratio_text = f"{ratio:.2f}x" if ratio is not None else "n/a"
            rss_text = (
                f"{result['peak_rss_kb'] / 1024:.1f}"
                if result.get("peak_rss_kb") is not None
                else "n/a"
            )
            bytes_per_entity = result.get("bytes_per_entity")
            bytes_per_entity_text = (
                f"{bytes_per_entity:.1f}" if bytes_per_entity is not None else "n/a"
            )
            lines.append(
                f"| {workload['name']} | {result['language']} | "
                f"{result['median_ms']:.3f} | {result['mad_ms']:.3f} | "
                f"{result['min_ms']:.3f} | {result['max_ms']:.3f} | "
                f"{result['work_units_per_sec'] / 1_000_000:.3f} | "
                f"{rss_text} | {bytes_per_entity_text} | {ratio_text} |"
            )
    if report["skipped"]:
        lines.extend(["", "## Skipped", ""])
        for item in report["skipped"]:
            lines.append(f"- {item['workload']} / {item['language']}: {item['reason']}")
    return "\n".join(lines) + "\n"


def parse_csv(value: str | None, allowed: list[str]) -> list[str]:
    if not value:
        return list(allowed)
    requested = [part.strip() for part in value.split(",") if part.strip()]
    unknown = sorted(set(requested) - set(allowed))
    if unknown:
        raise ValueError(f"unknown value(s): {', '.join(unknown)}")
    return requested


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--languages", help="comma-separated language filter")
    parser.add_argument("--workloads", help="comma-separated workload filter")
    parser.add_argument(
        "--nulang-bin",
        type=Path,
        help="release Nulang CLI; defaults to Cargo's configured target directory",
    )
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    args = parser.parse_args()

    if args.warmups < 0 or args.runs < 1:
        parser.error("--warmups must be >= 0 and --runs must be >= 1")

    suite = json.loads((SUITE_DIR / "suite.json").read_text())
    workload_defs = suite["workloads"]
    workload_names = [w["name"] for w in workload_defs]
    languages = parse_csv(args.languages, LANGUAGE_ORDER)
    selected_workloads = parse_csv(args.workloads, workload_names)
    nulang_bin = discover_nulang_bin(args.nulang_bin)

    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "cpu": cpu_model(),
            "python": platform.python_version(),
            "cpu_count": os.cpu_count(),
        },
        "toolchains": toolchain_versions(nulang_bin),
        "settings": {
            "warmups": args.warmups,
            "runs": args.runs,
            "timeout_seconds": args.timeout,
            "actor_scheduler_policy": {
                "nulang": "NULANG_SHARDS=1",
                "go": "GOMAXPROCS=1",
                "erlang": "ERL_FLAGS=+S 1:1",
            },
        },
        "workloads": [],
        "skipped": [],
    }

    with tempfile.TemporaryDirectory(prefix="nulang-crosslang-") as tmp:
        temp_root = Path(tmp)
        for definition in workload_defs:
            name = definition["name"]
            if name not in selected_workloads:
                continue
            workload_dir = SUITE_DIR / "workloads" / name
            workload_result: dict[str, Any] = {
                "name": name,
                "category": definition.get("category", "uncategorized"),
                "description": definition["description"],
                "iterations": definition["iterations"],
                "expected_stdout": definition["expected_stdout"],
                "results": [],
            }

            supported_languages = definition.get("languages", LANGUAGE_ORDER)
            is_actor_workload = definition.get("category") == "concurrency"
            for language in LANGUAGE_ORDER:
                if language not in languages or language not in supported_languages:
                    continue
                if not available(language, nulang_bin):
                    if language == "nulang":
                        print(
                            f"ERROR: Nulang CLI not found at {nulang_bin}; "
                            "build the release binary or pass --nulang-bin",
                            file=sys.stderr,
                        )
                        return 2
                    report["skipped"].append(
                        {"workload": name, "language": language, "reason": "toolchain unavailable"}
                    )
                    continue

                build_dir = temp_root / name / language
                build_dir.mkdir(parents=True, exist_ok=True)
                try:
                    cmd = build_command(language, workload_dir, build_dir, nulang_bin)
                    run_env: dict[str, str] = {}
                    if is_actor_workload:
                        if language == "nulang":
                            run_env["NULANG_SHARDS"] = "1"
                        elif language == "go":
                            run_env["GOMAXPROCS"] = "1"
                        elif language == "erlang":
                            run_env["ERL_FLAGS"] = "+S 1:1"

                    peak_rss_kb = measure_peak_rss_kb(
                        cmd,
                        expected=definition["expected_stdout"],
                        timeout=args.timeout,
                        env_overrides=run_env,
                    )
                    samples = sample_command(
                        cmd,
                        expected=definition["expected_stdout"],
                        warmups=args.warmups,
                        runs=args.runs,
                        timeout=args.timeout,
                        env_overrides=run_env,
                    )
                except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
                    print(f"ERROR {name}/{language}: {exc}", file=sys.stderr)
                    return 1

                stats = summarize(samples)
                result = {
                    "language": language,
                    **stats,
                    "work_units_per_sec": definition["iterations"] / (stats["median_ms"] / 1000.0),
                    "peak_rss_kb": peak_rss_kb,
                }
                workload_result["results"].append(result)
                print(
                    f"{name:20s} {language:8s} "
                    f"median={result['median_ms']:10.3f} ms "
                    f"min={result['min_ms']:10.3f} ms"
                )

            report["workloads"].append(workload_result)

    baseline_workload = next(
        (w for w in report["workloads"] if w["name"] == "actor_runtime_baseline"),
        None,
    )
    if baseline_workload is not None:
        baseline_rss = {
            result["language"]: result.get("peak_rss_kb")
            for result in baseline_workload["results"]
        }
        spawn_workload = next(
            (w for w in report["workloads"] if w["name"] == "actor_spawn_100k"),
            None,
        )
        if spawn_workload is not None:
            entities = spawn_workload["iterations"]
            for result in spawn_workload["results"]:
                baseline_kb = baseline_rss.get(result["language"])
                peak_kb = result.get("peak_rss_kb")
                if baseline_kb is not None and peak_kb is not None and entities > 0:
                    incremental_kb = max(0, peak_kb - baseline_kb)
                    result["incremental_peak_rss_kb"] = incremental_kb
                    result["bytes_per_entity"] = incremental_kb * 1024 / entities

    markdown = markdown_report(report)
    print("\n" + markdown)

    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(json.dumps(report, indent=2) + "\n")
    if args.markdown_out:
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text(markdown)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
