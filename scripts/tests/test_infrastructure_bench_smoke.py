import unittest

from scripts.infrastructure_bench_smoke import EXPECTED, verify_output


class InfrastructureBenchSmokeTests(unittest.TestCase):
    def test_complete_run(self):
        verify_output("\n".join(f"Testing {case}\nSuccess" for case in EXPECTED))

    def test_empty_or_filtered_out_run_fails(self):
        with self.assertRaises(ValueError):
            verify_output("Finished dev profile")

    def test_missing_success_fails(self):
        lines = [f"Testing {case}\nSuccess" for case in EXPECTED[:-1]]
        lines.append(f"Testing {EXPECTED[-1]}")
        with self.assertRaises(ValueError):
            verify_output("\n".join(lines))

    def test_duplicate_case_fails(self):
        lines = [f"Testing {case}\nSuccess" for case in EXPECTED]
        lines.append(lines[0])
        with self.assertRaises(ValueError):
            verify_output("\n".join(lines))

    def test_failed_case_cannot_steal_next_success(self):
        lines = [f"Testing {case}\nSuccess" for case in EXPECTED[2:]]
        lines.insert(0, f"Testing {EXPECTED[0]}\nTesting {EXPECTED[1]}\nSuccess")
        with self.assertRaises(ValueError):
            verify_output("\n".join(lines))


if __name__ == "__main__":
    unittest.main()
