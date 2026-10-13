#!/usr/bin/env python3
"""Exact-revision Nulang performance envelope. Linux, single controlled host.

Uses existing Nulang executable probes; never interprets Cargo build time as
language compile time. JIT timings exclude source compilation. Actor RSS is a
spawn delta; process max RSS is a different metric. No regression thresholds.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKLOAD = 'numeric_100000'


def run_process(command: list[str], *, cwd: Path, cpu: set[int] | None = None) -> dict:
    """Measure one isolated Linux child via wait4 (wall latency + true peak RSS)."""
    if not hasattr(os, 'wait4'):
        raise RuntimeError('Linux/Unix os.wait4 is required for child-specific peak RSS')

    def pin() -> None:
        if cpu is not None:
            os.sched_setaffinity(0, cpu)

    with tempfile.TemporaryFile(mode='w+b') as capture:
        started = time.perf_counter_ns()
        proc = subprocess.Popen(command, cwd=cwd, stdout=capture,
                                stderr=subprocess.STDOUT,
                                preexec_fn=pin if cpu is not None else None)
        _, status, usage = os.wait4(proc.pid, 0)
        elapsed_ns = time.perf_counter_ns() - started
        proc.returncode = os.waitstatus_to_exitcode(status)
        capture.seek(0)
        output = capture.read().decode('utf-8', errors='replace')
    if proc.returncode != 0:
        raise RuntimeError(f'{command[0]} exit {proc.returncode}: {output[-3000:]}')
    # Linux reports ru_maxrss in KiB. macOS reports bytes and is not supported.
    return {'elapsed_ns': elapsed_ns, 'peak_rss_bytes': int(usage.ru_maxrss) * 1024,
            'stdout': output}


def checked(command: list[str], *, cwd: Path = ROOT) -> str:
    return subprocess.check_output(command, cwd=cwd, text=True, stderr=subprocess.STDOUT).strip()


def summarize_process_samples(rows: list[dict]) -> dict:
    if not rows:
        raise ValueError('no samples')
    return {
        'samples': len(rows),
        'median_elapsed_ns': statistics.median(row['elapsed_ns'] for row in rows),
        'median_peak_rss_bytes': statistics.median(row['peak_rss_bytes'] for row in rows),
        'max_peak_rss_bytes': max(row['peak_rss_bytes'] for row in rows),
        'individual': [{'elapsed_ns': row['elapsed_ns'], 'peak_rss_bytes': row['peak_rss_bytes']}
                       for row in rows],
    }


def parse_jit_rows(output: str, workload: str, repeats: int) -> dict:
    rows = [json.loads(line) for line in output.splitlines() if line.strip()]
    if len(rows) != repeats:
        raise ValueError(f'expected {repeats} JIT rows, got {len(rows)}')
    for row in rows:
        if row.get('schema') != 2 or row.get('workload') != workload:
            raise ValueError('wrong workload or JIT telemetry schema')
        if row.get('compiled_regions', 0) <= 0:
            raise ValueError('no compiled regions: cannot label warm timing as JIT')
        for key in ('interpreter_run_ns', 'first_run_ns', 'warm_run_ns'):
            if not isinstance(row.get(key), int) or row[key] <= 0:
                raise ValueError(f'bad JIT {key}')
        if not isinstance(row.get('first_compile', {}).get('total_compile_ns'), int):
            raise ValueError('missing JIT codegen telemetry')
    return {
        'workload': workload, 'samples': rows,
        'median_interpreter_ns': statistics.median(r['interpreter_run_ns'] for r in rows),
        'median_first_jit_ns': statistics.median(r['first_run_ns'] for r in rows),
        'median_warm_jit_ns': statistics.median(r['warm_run_ns'] for r in rows),
        'median_jit_codegen_ns': statistics.median(r['first_compile']['total_compile_ns'] for r in rows),
    }


def parse_density(output: str, expected_actors: int) -> dict:
    fields = dict(line.strip().split('=', 1) for line in output.splitlines() if '=' in line)
    if fields.get('mode') != 'idle':
        raise ValueError('wrong actor-density mode')
    if int(fields.get('actor_count', '0')) != expected_actors:
        raise ValueError('wrong actor count')
    required = ('actor_struct_bytes', 'spawn_ns_per_item',
                'rss_spawn_delta_bytes_per_actor', 'rss_spawn_delta_bytes')
    for key in required:
        if key not in fields:
            raise ValueError(f'missing {key}')
    actor_bytes = int(fields['actor_struct_bytes'])
    if actor_bytes <= 0:
        raise ValueError('actor layout must be positive')
    rss = fields['rss_spawn_delta_bytes_per_actor']
    delta = fields['rss_spawn_delta_bytes']
    return {
        'actor_count': expected_actors,
        'actor_struct_bytes': actor_bytes,
        'spawn_ns_per_actor': float(fields['spawn_ns_per_item']),
        'rss_bytes_per_actor': None if rss == 'unavailable' or float(rss) == 0 else float(rss),
        'total_rss_delta_bytes': None if delta == 'unavailable' or int(delta) == 0 else int(delta),
    }


def parse_allocation_profile(output: str, allocations: int) -> dict:
    rows = [json.loads(line) for line in output.splitlines() if line.strip()]
    if not rows:
        raise ValueError('no allocation rows')
    seen = set()
    for row in rows:
        size = row['payload_bytes']
        if size in seen:
            raise ValueError(f'duplicate allocation size {size}')
        seen.add(size)
        if (row['allocations'] != allocations or row['payload_total_bytes'] != size * allocations
                or row['physical_bytes'] < row['payload_total_bytes']):
            raise ValueError('inconsistent allocation accounting')
    return {
        'fixture': 'synthetic_equal_count_per_size_not_production_trace',
        'buckets': rows,
        'total_payload_bytes': sum(r['payload_total_bytes'] for r in rows),
        'total_physical_bytes': sum(r['physical_bytes'] for r in rows),
    }


def chosen_cpu(mode: str) -> set[int] | None:
    if mode == 'host':
        return None
    if not hasattr(os, 'sched_getaffinity') or not hasattr(os, 'sched_setaffinity'):
        raise RuntimeError('single-CPU mode requires sched_getaffinity/sched_setaffinity')
    cores = sorted(os.sched_getaffinity(0))
    if not cores:
        raise RuntimeError('no available CPUs')
    return {cores[0]}


def run_suite(args: argparse.Namespace) -> dict:
    if platform.system() != 'Linux':
        raise RuntimeError('publishable peak-RSS measurements require Linux')
    if not shutil.which('cargo'):
        raise RuntimeError('Cargo is required to build measured executables')
    if not shutil.which('git'):
        raise RuntimeError('Git is required to identify the exact source revision')
    sha = checked(['git', 'rev-parse', 'HEAD'])
    if len(sha) != 40:
        raise RuntimeError('cannot resolve full commit SHA')
    dirty = bool(checked(['git', 'status', '--porcelain', '--untracked-files=no']))
    if dirty and not args.allow_dirty:
        raise RuntimeError('dirty tracked checkout: commit or use --allow-dirty (not publishable)')
    cpu = chosen_cpu(args.cpu_mode)
    fixture = ROOT / 'benchmarks' / 'perf_envelope_numeric.nula'
    fixture_hash = hashlib.sha256(fixture.read_bytes()).hexdigest()

    # Build before sampling so Cargo time / rustc RSS cannot contaminate results.
    checked(['cargo', 'build', '--locked', '--release', '--no-default-features',
             '--features', 'native-codegen', '--bin', 'nulang',
             '--bin', 'nulang-jit-bench', '--example', 'actor_density',
             '--example', 'actor_allocation_profile'])
    target = Path(json.loads(checked(['cargo', 'metadata', '--format-version', '1', '--no-deps']))['target_directory'])
    compiler = str(target / 'release' / 'nulang')
    jit = str(target / 'release' / 'nulang-jit-bench')
    density = str(target / 'release' / 'examples' / 'actor_density')
    allocations = str(target / 'release' / 'examples' / 'actor_allocation_profile')

    def repeat_command(command: list[str]) -> dict:
        for _ in range(args.warmup):
            run_process(command, cwd=ROOT, cpu=cpu)
        return summarize_process_samples([run_process(command, cwd=ROOT, cpu=cpu)
                                          for _ in range(args.repeats)])

    # Bytecode compilation (not execution), each invocation writes a fresh artifact.
    with tempfile.TemporaryDirectory(prefix='nulang-perf-') as temp:
        output = str(Path(temp) / 'program.nbc')
        compile_bytecode = repeat_command([compiler, '--emit-nbc', '--out', output, str(fixture)])
        if not Path(output).is_file() or Path(output).stat().st_size == 0:
            raise RuntimeError('--emit-nbc did not produce a nonempty artifact')
        artifact_bytes = Path(output).stat().st_size

    # Native CLI includes execution (not a pure compilation metric); never conflate.
    native_compile_run = repeat_command([compiler, '--backend', 'native', str(fixture)])

    jit_run = run_process([jit, '--format', 'jsonl', '--workload', WORKLOAD,
                           '--repeat', str(args.repeats)], cwd=ROOT, cpu=cpu)
    jit_results = parse_jit_rows(jit_run['stdout'], WORKLOAD, args.repeats)
    density_run = run_process([density, 'idle', str(args.actors)], cwd=ROOT, cpu=cpu)
    density_results = parse_density(density_run['stdout'], args.actors)
    alloc_run = run_process([allocations, str(args.allocations)], cwd=ROOT, cpu=cpu)
    allocation_results = parse_allocation_profile(alloc_run['stdout'], args.allocations)

    return {
        'schema': 1, 'timestamp_utc': datetime.now(timezone.utc).isoformat(),
        'commit_sha': sha, 'dirty': dirty,
        'publishable': not dirty and args.cpu_mode == 'single',
        'host': {'system': platform.platform(), 'machine': platform.machine(),
                 'cpu_model': next((line.split(':', 1)[1].strip() for line in
                     Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name')), None),
                 'pinned_cpus': sorted(cpu) if cpu else None,
                 'rustc': checked(['rustc', '--version']),
                 'cargo': checked(['cargo', '--version'])},
        'build': {'profile': 'release', 'default_features': False,
                  'features': ['native-codegen'], 'fixture_sha256': fixture_hash,
                  'fixture': str(fixture.relative_to(ROOT))},
        'method': {'repetitions': args.repeats, 'process_warmups': args.warmup,
                   'peak_rss': 'per-child wait4 ru_maxrss (Linux KiB)',
                   'language_compile_time': 'fresh CLI process + bytecode generation + file write',
                   'jit_time': 'in-process execution only; source frontend excluded',
                   'native_time': 'fresh CLI process including native compilation AND execution',
                   'density': 'spawn delta; not process peak RSS',
                   'allocations': 'synthetic size buckets; not a production allocation histogram'},
        'bytecode_compilation': {**compile_bytecode, 'artifact_bytes': artifact_bytes},
        'native_compile_and_run': native_compile_run,
        'jit': jit_results, 'density': density_results, 'allocation': allocation_results,
        'process_peak_rss_bytes': {'jit_runner': jit_run['peak_rss_bytes'],
                                    'density_runner': density_run['peak_rss_bytes'],
                                    'allocation_runner': alloc_run['peak_rss_bytes']},
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--warmup', type=int, default=1)
    parser.add_argument('--actors', type=int, default=100000)
    parser.add_argument('--allocations', type=int, default=10000)
    parser.add_argument('--cpu-mode', choices=['single', 'host'], default='single')
    parser.add_argument('--allow-dirty', action='store_true')
    args = parser.parse_args()
    if args.repeats <= 0 or args.warmup < 0 or args.actors <= 0 or args.allocations <= 0:
        parser.error('repeats, actors, allocations must be positive and warmup nonnegative')
    report = run_suite(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + '\n')
    print(f'Wrote {args.output} for {report["commit_sha"]}')


if __name__ == '__main__':
    main()
