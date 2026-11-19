"""Unit tests for deliberately scoped capability grants in conformance fixtures."""
import unittest

from conformance.fixture_options import build_command


class FixtureOptionsTest(unittest.TestCase):
    def test_default_is_unprivileged(self):
        self.assertEqual(build_command("nulang", "case.nula", {}), ["nulang", "case.nula"])

    def test_explicit_capability_is_forwarded_as_separate_argv(self):
        self.assertEqual(
            build_command("nulang", "case.nula", {"capabilities": ["fs", "net"]}),
            ["nulang", "--with", "fs,net", "case.nula"],
        )

    def test_unknown_capability_is_rejected(self):
        with self.assertRaises(ValueError):
            build_command("nulang", "case.nula", {"capabilities": ["fs", "root"]})

    def test_non_array_capabilities_are_rejected(self):
        for value in ["fs", None, {}, 5]:
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    build_command("nulang", "case.nula", {"capabilities": value})

    def test_duplicate_capabilities_are_rejected(self):
        with self.assertRaises(ValueError):
            build_command("nulang", "case.nula", {"capabilities": ["fs", "fs"]})


if __name__ == "__main__":
    unittest.main()
