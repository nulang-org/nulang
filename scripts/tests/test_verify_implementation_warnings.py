"""Offline contract tests for Cargo warning verification.

These use Cargo's documented JSON message protocol, not a Rust toolchain.
"""
import importlib.util
import json
from pathlib import Path
import subprocess
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "nulang_verify_implementation", ROOT / "scripts" / "verify_implementation.py"
)
verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verifier)


def cargo_stream(*messages, success=True, with_finish=True):
    records = [
        {
            "reason": "compiler-message",
            "message": {"level": level, "message": message, "rendered": message},
        }
        for level, message in messages
    ]
    if with_finish:
        records.append({"reason": "build-finished", "success": success})
    return "\n".join(json.dumps(record) for record in records) + "\n"


class CargoWarningProtocolTests(unittest.TestCase):
    def test_counts_warning_even_when_source_path_contains_warnings(self):
        stream = cargo_stream(
            ("warning", "src/warnings.rs:12:9: warning: unused variable")
        )
        self.assertEqual(len(verifier.parse_cargo_warning_messages(stream)), 1)

    def test_counts_warning_even_when_message_mentions_generated(self):
        stream = cargo_stream(
            ("warning", "src/generated.rs:5:1: warning: generated warnings")
        )
        self.assertEqual(len(verifier.parse_cargo_warning_messages(stream)), 1)

    def test_ignores_notes_and_build_artifacts(self):
        stream = cargo_stream(("note", "context"), ("warning", "unused binding"))
        self.assertEqual(len(verifier.parse_cargo_warning_messages(stream)), 1)

    def test_accepts_clean_successful_build(self):
        self.assertEqual(verifier.parse_cargo_warning_messages(cargo_stream()), [])

    def test_rejects_non_json_output(self):
        with self.assertRaisesRegex(ValueError, "non-JSON"):
            verifier.parse_cargo_warning_messages("not a cargo JSON message\n")

    def test_rejects_absent_build_finished_marker(self):
        with self.assertRaisesRegex(ValueError, "build-finished"):
            verifier.parse_cargo_warning_messages(cargo_stream(with_finish=False))

    def test_rejects_unsuccessful_build_finished_marker(self):
        with self.assertRaisesRegex(ValueError, "build-finished"):
            verifier.parse_cargo_warning_messages(cargo_stream(success=False))

    def test_rejects_incomplete_compiler_message(self):
        stream = json.dumps({"reason": "compiler-message", "message": {}}) + "\n"
        stream += cargo_stream()
        with self.assertRaisesRegex(ValueError, "compiler-message"):
            verifier.parse_cargo_warning_messages(stream)


class CargoWarningGateTests(unittest.TestCase):
    def test_fails_for_real_warning_even_if_text_looks_like_summary(self):
        result = subprocess.CompletedProcess(
            args=["cargo", "check"],
            returncode=0,
            stdout=cargo_stream(("warning", "src/warnings.rs:1:1: warning: generated")),
            stderr="",
        )
        with mock.patch.object(verifier.subprocess, "run", return_value=result) as run:
            self.assertFalse(verifier.check_warnings_for([], "(test)"))
        self.assertIn("--message-format=json", run.call_args.args[0])

    def test_passes_for_clean_successful_build(self):
        result = subprocess.CompletedProcess(
            args=["cargo", "check"], returncode=0,
            stdout=cargo_stream(), stderr="",
        )
        with mock.patch.object(verifier.subprocess, "run", return_value=result):
            self.assertTrue(verifier.check_warnings_for([], "(test)"))

    def test_rejects_nonzero_cargo_exit_even_with_success_marker(self):
        result = subprocess.CompletedProcess(
            args=["cargo", "check"], returncode=101,
            stdout=cargo_stream(), stderr="compiler error",
        )
        with mock.patch.object(verifier.subprocess, "run", return_value=result):
            self.assertFalse(verifier.check_warnings_for([], "(test)"))

    def test_rejects_truncated_cargo_json_stream(self):
        result = subprocess.CompletedProcess(
            args=["cargo", "check"], returncode=0,
            stdout=cargo_stream(with_finish=False), stderr="",
        )
        with mock.patch.object(verifier.subprocess, "run", return_value=result):
            self.assertFalse(verifier.check_warnings_for([], "(test)"))


if __name__ == "__main__":
    unittest.main()
