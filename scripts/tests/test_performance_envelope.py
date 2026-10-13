import importlib.util
import json
import pathlib
import sys
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / 'performance_envelope.py'
spec = importlib.util.spec_from_file_location('performance_envelope', SCRIPT)
perf = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = perf
spec.loader.exec_module(perf)


class ReportingTests(unittest.TestCase):
    def test_jit_rejects_mismatched_workload(self):
        row = {'schema': 2, 'workload': 'numeric_1000', 'interpreter_run_ns': 100,
               'first_run_ns': 120, 'warm_run_ns': 50, 'compiled_regions': 1,
               'first_compile': {'total_compile_ns': 20}}
        with self.assertRaisesRegex(ValueError, 'wrong workload'):
            perf.parse_jit_rows(json.dumps(row), 'numeric_100000', 1)

    def test_jit_rejects_zero_compilation(self):
        row = {'schema': 2, 'workload': 'numeric_100000', 'interpreter_run_ns': 100,
               'first_run_ns': 120, 'warm_run_ns': 50, 'compiled_regions': 0,
               'first_compile': {'total_compile_ns': 0}}
        with self.assertRaisesRegex(ValueError, 'no compiled regions'):
            perf.parse_jit_rows(json.dumps(row), 'numeric_100000', 1)

    def test_jit_medians_and_compiler_time_are_independent(self):
        def make(t, c):
            return json.dumps({'schema': 2, 'workload': 'numeric_100000',
                 'interpreter_run_ns': t * 2, 'first_run_ns': t * 3,
                 'warm_run_ns': t, 'compiled_regions': 1,
                 'first_compile': {'total_compile_ns': c}})
        rows = perf.parse_jit_rows('\n'.join([make(100, 40), make(200, 60), make(300, 80)]), 'numeric_100000', 3)
        self.assertEqual(rows['median_interpreter_ns'], 400)
        self.assertEqual(rows['median_warm_jit_ns'], 200)
        self.assertEqual(rows['median_jit_codegen_ns'], 60)
        self.assertEqual(len(rows['samples']), 3)

    def test_density_rejects_missing_rss(self):
        with self.assertRaisesRegex(ValueError, 'missing rss_spawn_delta_bytes_per_actor'):
            perf.parse_density('mode=idle\nactor_count=100000\nactor_struct_bytes=1000\nspawn_ns_per_item=5000\n', 100000)

    def test_density_marks_unavailable_rss_not_zero(self):
        output = 'mode=idle\nactor_count=100000\nactor_struct_bytes=1000\n' \
                 'spawn_ns_per_item=5000\nrss_spawn_delta_bytes_per_actor=unavailable\n' \
                 'rss_spawn_delta_bytes=0\n'
        row = perf.parse_density(output, 100000)
        self.assertIsNone(row['rss_bytes_per_actor'])

    def test_density_rejects_wrong_actor_population(self):
        output = 'mode=idle\nactor_count=10000\nactor_struct_bytes=1000\n' \
                 'spawn_ns_per_item=5000\nrss_spawn_delta_bytes_per_actor=2000\n' \
                 'rss_spawn_delta_bytes=20000000\n'
        with self.assertRaisesRegex(ValueError, 'actor count'):
            perf.parse_density(output, 100000)

    def test_allocation_distribution_preserves_physical_overhead(self):
        output = '\n'.join([
          json.dumps({'payload_bytes': 16, 'allocations': 100, 'physical_bytes': 12800, 'payload_total_bytes': 1600}),
          json.dumps({'payload_bytes': 64, 'allocations': 100, 'physical_bytes': 12800, 'payload_total_bytes': 6400}),
        ])
        data = perf.parse_allocation_profile(output, 100)
        self.assertEqual(data['total_physical_bytes'], 25600)
        self.assertEqual(data['total_payload_bytes'], 8000)
        self.assertEqual(len(data['buckets']), 2)

    def test_allocation_distribution_rejects_duplicated_sizes(self):
        row = json.dumps({'payload_bytes':16, 'allocations':10, 'physical_bytes':1280, 'payload_total_bytes':160})
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            perf.parse_allocation_profile(row + '\n' + row, 10)

    def test_wait4_obtains_child_specific_peak_rss(self):
        sample = perf.run_process([sys.executable, '-c', 'print("ok")'], cwd=pathlib.Path.cwd())
        self.assertIn('ok', sample['stdout'])
        self.assertGreater(sample['elapsed_ns'], 0)
        self.assertGreater(sample['peak_rss_bytes'], 0)

    def test_wait4_nonzero_exit_fails_closed(self):
        with self.assertRaisesRegex(RuntimeError, 'exit 7'):
            perf.run_process([sys.executable, '-c', 'import sys; sys.exit(7)'], cwd=pathlib.Path.cwd())

    def test_summarize_compiles_never_confuses_rss_and_time(self):
        rows = [{'elapsed_ns': 100, 'peak_rss_bytes': 10000},
                {'elapsed_ns': 300, 'peak_rss_bytes': 20000},
                {'elapsed_ns': 200, 'peak_rss_bytes': 15000}]
        result = perf.summarize_process_samples(rows)
        self.assertEqual(result['median_elapsed_ns'], 200)
        self.assertEqual(result['median_peak_rss_bytes'], 15000)
        self.assertEqual(result['max_peak_rss_bytes'], 20000)


if __name__ == '__main__':
    unittest.main()
