"""Contract tests for optional Nulang actor enqueue/drain timing evidence."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from cross_runtime_phases import parse_nulang_phases


class CrossRuntimePhaseTests(unittest.TestCase):
    def test_valid_phases_match_total_and_preserve_workload_identity(self):
        output = (
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=45 drain_ns=55\n'
            '[cross-phase] runtime=nulang benchmark=fork_join enqueue_ns=90 drain_ns=210\n'
        )
        result = parse_nulang_phases(output, {'counting': 100, 'fork_join': 300})
        self.assertEqual(result['counting'], {'enqueue_ns': 45, 'drain_ns': 55})
        self.assertEqual(result['fork_join'], {'enqueue_ns': 90, 'drain_ns': 210})

    def test_missing_phase_fails_closed(self):
        with self.assertRaisesRegex(ValueError, 'missing'):
            parse_nulang_phases('[cross-phase] runtime=nulang benchmark=counting enqueue_ns=1 drain_ns=2', {'counting': 3, 'fork_join': 6})

    def test_duplicate_phase_fails_closed(self):
        output = (
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=1 drain_ns=2\n'
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=1 drain_ns=2\n'
            '[cross-phase] runtime=nulang benchmark=fork_join enqueue_ns=2 drain_ns=4\n'
        )
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            parse_nulang_phases(output, {'counting': 3, 'fork_join': 6})

    def test_inconsistent_total_fails_closed(self):
        output = (
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=1 drain_ns=2\n'
            '[cross-phase] runtime=nulang benchmark=fork_join enqueue_ns=2 drain_ns=4\n'
        )
        with self.assertRaisesRegex(ValueError, 'does not equal'):
            parse_nulang_phases(output, {'counting': 4, 'fork_join': 6})

    def test_malformed_and_unexpected_phase_fails_closed(self):
        for invalid in [
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=1',
            '[cross-phase] runtime=go benchmark=counting enqueue_ns=1 drain_ns=2',
            '[cross-phase] runtime=nulang benchmark=ping_pong enqueue_ns=1 drain_ns=2',
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=-1 drain_ns=4',
        ]:
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                parse_nulang_phases(invalid, {'counting': 3, 'fork_join': 6})

    def test_bounded_zero_duration_allowed(self):
        output = (
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=0 drain_ns=2\n'
            '[cross-phase] runtime=nulang benchmark=fork_join enqueue_ns=1 drain_ns=3'
        )
        result = parse_nulang_phases(output, {'counting': 2, 'fork_join': 4})
        self.assertEqual(result['counting']['enqueue_ns'], 0)


if __name__ == '__main__':
    unittest.main()
