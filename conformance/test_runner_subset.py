"""Keep targeted stdlib gating explicit and reject missing or duplicated names."""
import unittest

from conformance.fixture_options import select_fixture_names


class ConformanceSubsetTest(unittest.TestCase):
    def test_no_selection_runs_all_in_stable_order(self):
        self.assertEqual(select_fixture_names(["b", "a"], None), ["a", "b"])

    def test_subset_uses_exact_fixture_names(self):
        self.assertEqual(select_fixture_names(["b", "a", "c"], ["c", "a"]), ["a", "c"])

    def test_unknown_selection_fails_closed(self):
        with self.assertRaises(ValueError):
            select_fixture_names(["valid"], ["missing"])

    def test_duplicate_selection_fails_closed(self):
        with self.assertRaises(ValueError):
            select_fixture_names(["valid"], ["valid", "valid"])

    def test_empty_explicit_selection_fails_closed(self):
        with self.assertRaises(ValueError):
            select_fixture_names(["valid"], [])


if __name__ == "__main__":
    unittest.main()
