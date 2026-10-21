"""Contract tests for packaged release archive verification (no compiler required)."""
import hashlib
import io
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from verify_release_artifact import VerificationError, verify_artifact


@unittest.skipIf(os.name == "nt", "fixture uses a POSIX shell executable")
class ReleaseArtifactVerificationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.member = "nulang-linux-x86_64"
        self.archive = self.root / (self.member + ".tar.gz")
        self.checksum = self.root / (self.member + ".tar.gz.sha256")
        self.script = (
            "#!/bin/sh\n"
            'if [ "$1" = "--version" ]; then echo "Nulang 0.1.0"; exit 0; fi\n'
            'if [ "$1" = "--eval" ]; then echo "release smoke ok"; exit 0; fi\n'
            "exit 2\n"
        )
        self.make_archive()

    def make_archive(self, script=None, extra_member=False, member=None):
        script = self.script if script is None else script
        data = script.encode("utf-8")
        with tarfile.open(self.archive, mode="w:gz") as archive:
            entry = tarfile.TarInfo(member or self.member)
            entry.mode = 0o755
            entry.size = len(data)
            archive.addfile(entry, io.BytesIO(data))
            if extra_member:
                unwanted = tarfile.TarInfo("../unwanted")
                unwanted.size = 1
                archive.addfile(unwanted, io.BytesIO(b"x"))
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        self.checksum.write_text(f"{digest}  {self.archive.name}\n")

    def verify(self, execute=True, tag="v0.1.0"):
        verify_artifact(self.archive, self.checksum, self.member, tag, execute=execute)

    def test_passes_valid_extracted_native_binary(self):
        self.verify()

    def test_cross_compiled_archive_can_be_checked_without_execution(self):
        self.verify(execute=False)

    def test_rejects_changed_archive_after_checksum_generation(self):
        self.archive.write_bytes(self.archive.read_bytes() + b"garbage")
        with self.assertRaisesRegex(VerificationError, "checksum"):
            self.verify(execute=False)

    def test_rejects_unexpected_archive_member(self):
        self.make_archive(extra_member=True)
        with self.assertRaisesRegex(VerificationError, "exactly one"):
            self.verify(execute=False)

    def test_rejects_missing_expected_binary_member(self):
        self.make_archive(member="wrong-name")
        with self.assertRaisesRegex(VerificationError, "expected binary"):
            self.verify(execute=False)

    def test_rejects_wrong_cli_version(self):
        with self.assertRaisesRegex(VerificationError, "version"):
            self.verify(tag="v0.2.0")

    def test_rejects_version_embedded_in_another_version(self):
        # v0.1.0 must not accept "Nulang 10.1.0" just because its
        # version digits contain the requested version as a substring.
        self.make_archive(script=self.script.replace("Nulang 0.1.0", "Nulang 10.1.0"))
        with self.assertRaisesRegex(VerificationError, "version"):
            self.verify()

    def test_rejects_version_with_unexpected_suffix(self):
        self.make_archive(script=self.script.replace("Nulang 0.1.0", "Nulang 0.1.0-debug"))
        with self.assertRaisesRegex(VerificationError, "version"):
            self.verify()

    def test_accepts_version_line_with_language_metadata(self):
        self.make_archive(script=self.script.replace(
            'echo "Nulang 0.1.0"; exit 0;',
            'printf "nulang 0.1.0\\nlanguage 1.0.0-frozen\\n"; exit 0;',
        ))
        self.verify()

    def test_rejects_broken_eval_command(self):
        self.make_archive(script="#!/bin/sh\nexit 5\n")
        with self.assertRaises(VerificationError):
            self.verify()

    def test_rejects_checksum_for_another_filename(self):
        self.checksum.write_text(
            self.checksum.read_text().replace(self.archive.name, "unrelated.tar.gz")
        )
        with self.assertRaisesRegex(VerificationError, "filename"):
            self.verify(execute=False)


if __name__ == "__main__":
    unittest.main()
