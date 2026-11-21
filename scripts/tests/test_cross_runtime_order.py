"""Tests for balanced cross-runtime benchmark measurement ordering."""

from __future__ import annotations

import sys
import unittest
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cross_runtime_order import counterbalanced_runtime_order


class CounterbalancedRuntimeOrderTests(unittest.TestCase):
    def test_four_runtimes_get_equal_exposure_to_every_position(self):
        runtimes = ["nulang", "rust", "go", "erlang"]
        rounds = [counterbalanced_runtime_order(runtimes, i) for i in range(8)]

        for runtime in runtimes:
            positions = Counter(order.index(runtime) for order in rounds)
            self.assertEqual(positions, Counter({0: 2, 1: 2, 2: 2, 3: 2}))

    def test_three_runtimes_are_balanced_after_six_rounds(self):
        runtimes = ["nulang", "go", "erlang"]
        rounds = [counterbalanced_runtime_order(runtimes, i) for i in range(6)]

        for runtime in runtimes:
            self.assertEqual(
                Counter(order.index(runtime) for order in rounds),
                Counter({0: 2, 1: 2, 2: 2}),
            )

    def test_adjacent_rounds_reverse_the_same_rotation(self):
        runtimes = ["nulang", "rust", "go", "erlang"]
        for pair in range(4):
            forward = counterbalanced_runtime_order(runtimes, 2 * pair)
            reverse = counterbalanced_runtime_order(runtimes, 2 * pair + 1)
            self.assertEqual(reverse, list(reversed(forward)))

    def test_preserves_input_and_handles_single_runtime(self):
        runtimes = ["nulang", "go"]
        before = runtimes.copy()
        self.assertEqual(counterbalanced_runtime_order(["nulang"], 19), ["nulang"])
        self.assertEqual(counterbalanced_runtime_order([], 0), [])
        self.assertEqual(counterbalanced_runtime_order(runtimes, 2), ["go", "nulang"])
        self.assertEqual(runtimes, before)


if __name__ == "__main__":
    unittest.main()
