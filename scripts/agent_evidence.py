#!/usr/bin/env python3
"""Collect reproducible local Git/command evidence without recording command output.

This is NOT an independent reviewer, CI status fetcher, or merge gate.
No commands run unless explicitly requested via --check; no shell is used.
"""
import argparse
import hashlib
import json
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path

SHA = re.compile(r'^[a-f0-9]{40}$')


def git(cwd, *args):
    proc = subprocess.run(['git', *args], cwd=cwd, capture_output=True, text=True)
    if proc.returncode:
        raise ValueError(f'git {args[0]} failed (exit {proc.returncode})')
    return proc.stdout.strip()


def collect(cwd, base, checks, timeout=600):
    """Build an allowlisted packet; each check is (name, argv), never shell text."""
    cwd = Path(cwd).resolve()
    if not isinstance(base, str) or not SHA.fullmatch(base):
        raise ValueError('base SHA must be a full immutable 40-character hex commit')
    git(cwd, 'cat-file', '-e', f'{base}^{{commit}}')
    head = git(cwd, 'rev-parse', 'HEAD')
    if not SHA.fullmatch(head):
        raise ValueError('unable to resolve complete head SHA')
    changed = git(cwd, 'diff', '--name-only', f'{base}...{head}').splitlines()
    dirty = bool(git(cwd, 'status', '--porcelain', '--untracked-files=all'))
    if len(checks) > 20 or len({name for name, _ in checks}) != len(checks):
        raise ValueError('checks must have unique names and at most 20 entries')
    results = []
    for name, argv in checks:
        if not isinstance(name, str) or not re.fullmatch(r'[a-zA-Z0-9_.-]{1,80}', name):
            raise ValueError('check names must be short alphanumeric identifiers')
        if not isinstance(argv, (list, tuple)) or not argv or not all(isinstance(x, str) and x for x in argv):
            raise ValueError('check command must be a non-empty argument array')
        started = time.monotonic()
        try:
            proc = subprocess.run(argv, cwd=cwd, stdin=subprocess.DEVNULL,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  timeout=timeout)
            exit_code = proc.returncode
            status = 'passed' if exit_code == 0 else 'failed'
        except subprocess.TimeoutExpired:
            exit_code, status = 124, 'timeout'
        except OSError:
            exit_code, status = 127, 'unavailable'
        results.append({
            'name': name,
            'command_sha256': hashlib.sha256(json.dumps(argv, separators=(',', ':')).encode()).hexdigest(),
            'status': status,
            'exit_code': exit_code,
            'duration_ms': int((time.monotonic() - started) * 1000),
        })
    final_head = git(cwd, 'rev-parse', 'HEAD')
    dirty = dirty or bool(git(cwd, 'status', '--porcelain', '--untracked-files=all'))
    head_integrity = 'unchanged' if final_head == head else 'changed-during-checks'
    return {
        'schema_version': 1,
        'source': 'nulang-local-checks',
        'base_sha': base,
        'head_sha': head,
        'changed_files': sorted(changed),
        'dirty': dirty,
        'head_integrity': head_integrity,
        'checks': results,
        'ready_for_review': bool(results) and not dirty and head_integrity == 'unchanged' and all(r['status'] == 'passed' for r in results),
        'independent_review': {'status': 'unknown'},
        'ci': {'status': 'unknown'},
        'benchmark': {'status': 'not-measured'},
        'note': 'Local check outcomes only; CI, independent review and performance remain unverified.',
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base', required=True, help='Exact full base commit SHA')
    parser.add_argument('--check', action='append', default=[], metavar='NAME=COMMAND',
                        help='Run an explicitly named command without a shell (repeatable)')
    parser.add_argument('--output', type=Path, help='Write packet to a local file; defaults to stdout')
    parser.add_argument('--timeout', type=int, default=600, help='Per-check timeout in seconds (1-3600)')
    args = parser.parse_args()
    if not 1 <= args.timeout <= 3600:
        parser.error('--timeout must be between 1 and 3600 seconds')
    checks = []
    try:
        for check in args.check:
            if '=' not in check:
                raise ValueError('check must be NAME=COMMAND')
            name, cmd = check.split('=', 1)
            checks.append((name, shlex.split(cmd)))
        evidence = collect(Path.cwd(), args.base, checks, args.timeout)
        payload = json.dumps(evidence, indent=2, sort_keys=True) + '\n'
        if args.output:
            args.output.write_text(payload, encoding='utf-8')
        else:
            sys.stdout.write(payload)
        return 0 if evidence['ready_for_review'] else 1
    except (ValueError, OSError) as exc:
        print(f'agent_evidence: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
