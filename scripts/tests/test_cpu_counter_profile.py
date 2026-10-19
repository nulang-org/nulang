"""Contract tests for the opt-in native CPU counter profiler (no perf needed)."""
import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "cpu_counter_profile.py"
spec = importlib.util.spec_from_file_location("cpu_counter_profile", SCRIPT)
profile = importlib.util.module_from_spec(spec)
spec.loader.exec_module(profile)


class CounterParsingTests(unittest.TestCase):
    def test_parse_supported_counters_and_unavailable_values(self):
        text = (
            "123456;;cycles;1000000;100.00;;\n"
            "456789;;instructions;1000000;100.00;;\n"
            "<not supported>;;branch-misses;0;0.00;;\n"
            "# comment\n"
        )
        self.assertEqual(profile.parse_stat(text), {
            "cycles": 123456, "instructions": 456789, "branch-misses": None,
        })

    def test_unknown_events_are_not_reported_as_counters(self):
        self.assertEqual(profile.parse_stat("99;;unknown;100;;\n"), {})

    def test_non_numeric_counts_are_not_invented(self):
        self.assertEqual(profile.parse_stat("<not counted>;;cycles;0;;\n"), {"cycles": None})

    def test_cli_requires_explicit_command_and_valid_repetition(self):
        for args in [[], ["--", "true"], ["--output", "x", "--repeat", "0", "--", "true"]]:
            with self.subTest(args=args), self.assertRaises(SystemExit) as exited:
                profile.main(args)
            self.assertEqual(exited.exception.code, 2)

    def test_missing_perf_fails_before_creating_output(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(profile.shutil, "which", return_value=None):
            dest = pathlib.Path(tmp) / "metrics"
            with self.assertRaisesRegex(RuntimeError, "perf is required"):
                profile.collect(dest, ["true"], 1, 0, pathlib.Path(tmp))
            self.assertFalse(dest.exists())

    def test_performance_record_manifest_from_fake_perf(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fake = root / "perf"
            fake.write_text("""#!/usr/bin/env python3
import pathlib, sys
if '--version' in sys.argv:
    print('perf mock 1')
else:
    i = sys.argv.index('-o')
    pathlib.Path(sys.argv[i+1]).write_text('1000;;cycles;100;;\\n2000;;instructions;100;;\\n')
""")
            fake.chmod(0o755)
            dest = root / "profile"
            with mock.patch.object(profile.shutil, "which", return_value=str(fake)), \
                 mock.patch.object(profile, "repo_sha", return_value="012345"):
                result = profile.collect(dest, [sys.executable, "-c", "pass"], 2, 1, root)
            saved = json.loads((dest / "manifest.json").read_text())
            self.assertEqual(saved, result)
            self.assertEqual(saved["source_sha"], "012345")
            self.assertEqual(len(saved["runs"]), 2)
            self.assertEqual(saved["runs"][0]["counters"]["cycles"], 1000)
            with mock.patch.object(profile.shutil, "which", return_value=str(fake)):
                with self.assertRaisesRegex(RuntimeError, "refusing overwrite"):
                    profile.collect(dest, ["true"], 1, 0, root)

    def test_missing_counters_fail_closed_and_preserve_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fake = root / "perf"
            fake.write_text("#!/usr/bin/env python3\nimport sys\n")
            fake.chmod(0o755)
            dest = root / "profile"
            with mock.patch.object(profile.shutil, "which", return_value=str(fake)):
                with self.assertRaisesRegex(RuntimeError, "unavailable"):
                    profile.collect(dest, ["true"], 1, 0, root)
            saved = json.loads((dest / "manifest.json").read_text())
            self.assertEqual(len(saved["runs"]), 1)
            self.assertEqual(saved["runs"][0]["counters"], {})


if __name__ == "__main__":
    unittest.main()
