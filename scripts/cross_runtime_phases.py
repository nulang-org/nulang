"""Strict phase-evidence parser for Nulang's burst-then-drain actor baselines.

Only counting and fork_join have an externally supplied burst that can be
measured independently of actor scheduling. Other workloads remain unsplit.
"""

from __future__ import annotations

import re

_PHASE_WORKLOADS = frozenset({'counting', 'fork_join'})
_PHASE_RE = re.compile(
    r'\[cross-phase\] runtime=nulang benchmark=(counting|fork_join) '
    r'enqueue_ns=(\d+) drain_ns=(\d+)'
)


def parse_nulang_phases(
    output: str, totals_ns: dict[str, int]
) -> dict[str, dict[str, int]]:
    """Validate all required Nulang phase records against elapsed totals.

    The phases are *diagnostic components* of the same elapsed wall-clock
    measurement, never separately sampled throughput numbers.
    """
    expected = _PHASE_WORKLOADS & totals_ns.keys()
    phases: dict[str, dict[str, int]] = {}
    for line in output.splitlines():
        if not line.startswith('[cross-phase]'):
            continue
        match = _PHASE_RE.fullmatch(line)
        if match is None:
            raise ValueError(f'malformed or unexpected Nulang phase record: {line}')
        name, enqueue, drain = match.groups()
        if name not in expected:
            raise ValueError(f'unexpected phase workload: {name}')
        if name in phases:
            raise ValueError(f'duplicate phase record: {name}')
        enqueue_ns, drain_ns = int(enqueue), int(drain)
        if enqueue_ns + drain_ns != totals_ns[name]:
            raise ValueError(
                f'{name}: enqueue_ns + drain_ns does not equal elapsed_ns '
                f'({enqueue_ns} + {drain_ns} != {totals_ns[name]})'
            )
        phases[name] = {'enqueue_ns': enqueue_ns, 'drain_ns': drain_ns}
    missing = sorted(expected - phases.keys())
    if missing:
        raise ValueError(f'missing Nulang phase records: {", ".join(missing)}')
    return phases
