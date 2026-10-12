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



class CrossRuntimeRunnerIntegrationTests(unittest.TestCase):
    def test_phases_preserved_in_samples_and_summary(self):
        from cross_runtime_bench import parse_records, summarize

        output = '\n'.join([
            '[cross-bench] runtime=nulang benchmark=counting messages=200000 elapsed_ns=100',
            '[cross-bench] runtime=nulang benchmark=ping_pong messages=40001 elapsed_ns=200',
            '[cross-bench] runtime=nulang benchmark=thread_ring messages=20000 elapsed_ns=300',
            '[cross-bench] runtime=nulang benchmark=fork_join messages=100000 elapsed_ns=600',
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=20 drain_ns=80',
            '[cross-phase] runtime=nulang benchmark=fork_join enqueue_ns=250 drain_ns=350',
        ])
        records = parse_records(output, 'nulang')
        self.assertEqual(records['counting']['enqueue_ns'], 20)
        self.assertEqual(records['fork_join']['drain_ns'], 350)
        summary = summarize({'nulang': {name: [row] for name, row in records.items()}})
        self.assertEqual(summary['nulang']['counting']['median_enqueue_ns'], 20)
        self.assertEqual(summary['nulang']['fork_join']['median_drain_ns'], 350)

    def test_runner_rejects_incorrect_phase_sum(self):
        from cross_runtime_bench import parse_records

        output = '\n'.join([
            '[cross-bench] runtime=nulang benchmark=counting messages=200000 elapsed_ns=100',
            '[cross-bench] runtime=nulang benchmark=ping_pong messages=40001 elapsed_ns=200',
            '[cross-bench] runtime=nulang benchmark=thread_ring messages=20000 elapsed_ns=300',
            '[cross-bench] runtime=nulang benchmark=fork_join messages=100000 elapsed_ns=600',
            '[cross-phase] runtime=nulang benchmark=counting enqueue_ns=20 drain_ns=79',
            '[cross-phase] runtime=nulang benchmark=fork_join enqueue_ns=250 drain_ns=350',
        ])
        with self.assertRaisesRegex(ValueError, 'does not equal'):
            parse_records(output, 'nulang')


if __name__ == '__main__':
    unittest.main()
