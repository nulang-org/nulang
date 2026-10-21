#!/usr/bin/env python3
"""Reject known vacuous Lean effect-safety proof regressions.

This is a claim-integrity check, not a Lean kernel or soundness checker.
`lake build` remains mandatory to validate actual proofs.
"""
from pathlib import Path
import re
import sys


def check(source: str) -> list[str]:
    failures = []
    for name in ('effect_safety', 'effect_safety_static'):
        match = re.search(rf'\btheorem\s+{name}\b(?P<signature>.*?)\s*:=\s*by\b', source, re.S)
        if match is None:
            failures.append(f'{name} must be a proved theorem with a proof body')
            continue
        signature = match.group('signature')
        if not re.search(r'\bUnhandled\b', signature):
            failures.append(f'{name} must exclude a dynamically unhandled effect')
        if re.search(r':\s*True\s*$', signature):
            failures.append(f'{name} has a vacuous True conclusion')
    if re.search(r'(?m)^\s*(?:sorry|admit)\b', source):
        failures.append('effects.lean contains an unfinished proof')
    if re.search(r'(?m)^\s*axiom\s', source):
        failures.append('effects.lean contains an unproved axiom')
    return failures


def main() -> int:
    source = Path(__file__).resolve().parents[1] / 'spec/formal/effects.lean'
    failures = check(source.read_text(encoding='utf-8'))
    for failure in failures:
        print(f'error: {failure}', file=sys.stderr)
    if failures:
        return 1
    print('Effect-safety theorem signatures are non-vacuous; Lean build required.')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
