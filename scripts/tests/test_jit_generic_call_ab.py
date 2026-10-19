import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import jit_generic_call_ab as ab


def record(workload, warm_ns, *, first_ns=110, interp_ns=220):
    return {"schema": 2, "suite": "jit-codegen", "workload": workload,
            "iteration": 1, "interpreter_run_ns": interp_ns,
            "first_run_ns": first_ns, "warm_run_ns": warm_ns,
            "first_compile": {"total_compile_ns": 17},
            "warm_compile_delta": {"total_compile_ns": 8}}


class GenericCallAbTests(unittest.TestCase):
    def test_counterbalanced_order_and_per_round_pairing(self):
        calls = []

        def run(name):
            calls.append(name)
            return record(name, 100 if name == ab.CONCRETE else 120)

        summary, samples = ab.measure_pairs(5, run)
        self.assertEqual(calls, [ab.CONCRETE, ab.GENERIC, ab.GENERIC, ab.CONCRETE,
                                 ab.CONCRETE, ab.GENERIC, ab.GENERIC, ab.CONCRETE,
                                 ab.CONCRETE, ab.GENERIC])
        self.assertEqual(len(samples), 5)
        self.assertEqual(summary["rounds"], 5)
        self.assertEqual(summary["median_warm_generic_to_concrete_ratio"], 1.2)
        self.assertEqual(summary["median_warm_ns"][ab.GENERIC], 120)
        self.assertTrue(summary["observational_only"])

    def test_never_declares_a_win_from_timing(self):
        summary, _ = ab.measure_pairs(6, lambda n: record(n, 10 if n == ab.GENERIC else 100))
        self.assertNotIn("winner", summary)
        self.assertNotIn("significant", summary)

    def test_rejects_wrong_workload(self):
        def invalid(_name):
            return record("numeric_1000", 120)
        with self.assertRaisesRegex(ValueError, "workload"):
            ab.measure_pairs(5, invalid)

    def test_rejects_bad_schema_or_iteration(self):
        for field, value in (("schema", 1), ("iteration", 2), ("suite", "other")):
            with self.subTest(field=field):
                def invalid(name):
                    r = record(name, 100)
                    r[field] = value
                    return r
                with self.assertRaises(ValueError):
                    ab.measure_pairs(5, invalid)

    def test_rejects_zero_or_nonnumeric_durations(self):
        for value in (0, -1, "100", None, True):
            with self.subTest(value=value):
                def invalid(name):
                    r = record(name, 100)
                    r["warm_run_ns"] = value
                    return r
                with self.assertRaisesRegex(ValueError, "warm_run_ns"):
                    ab.measure_pairs(5, invalid)

    def test_rejects_insufficient_rounds(self):
        with self.assertRaisesRegex(ValueError, "at least 5"):
            ab.measure_pairs(4, lambda name: record(name, 100))

    def test_rejects_missing_compile_stats(self):
        def invalid(name):
            r = record(name, 100)
            del r["warm_compile_delta"]
            return r
        with self.assertRaises(ValueError):
            ab.measure_pairs(5, invalid)

    def test_parses_exactly_one_jsonl_benchmark_record(self):
        raw = json.dumps(record(ab.CONCRETE, 100)) + "\n"
        self.assertEqual(ab.parse_single_record(raw, ab.CONCRETE)["warm_run_ns"], 100)
        with self.assertRaisesRegex(ValueError, "exactly one"):
            ab.parse_single_record(raw + raw, ab.CONCRETE)
        with self.assertRaises(ValueError):
            ab.parse_single_record("junk", ab.CONCRETE)


if __name__ == "__main__":
    unittest.main()
