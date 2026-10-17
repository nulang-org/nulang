"""Counterbalanced execution order for same-host language/runtime benchmarks."""

from __future__ import annotations

from collections.abc import Sequence


def counterbalanced_runtime_order(runtimes: Sequence[str], round_index: int) -> list[str]:
    """Rotate each pair of measured rounds, reversing the second run.

    Over ``2 * len(runtimes)`` rounds, each runtime occupies each execution
    position exactly twice. This reduces correlation between runtime identity
    and within-round thermal, scheduling, or host-load drift.
    """
    if round_index < 0:
        raise ValueError("round_index must be non-negative")

    order = list(runtimes)
    if not order:
        return order

    rotation, reverse = divmod(round_index, 2)
    shift = rotation % len(order)
    order = order[shift:] + order[:shift]
    return order[::-1] if reverse else order
