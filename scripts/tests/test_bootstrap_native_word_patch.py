"""Cross-check one Nulang-written word patcher against independently pinned words.

The native implementation is deliberately not wired into the bootstrap pipeline.
The existing Python fixup remains authoritative until full stream parity is proven.
"""

import os
from pathlib import Path
import re
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]


class NativeWordPatchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        path = os.environ.get("NULANG_BIN", "")
        if not path or not Path(path).is_file():
            raise RuntimeError("NULANG_BIN must point to an existing compiled nulang executable")
        cls.binary = str(Path(path).resolve())

    def patched(self, request):
        process = subprocess.run(
            [self.binary, "bootstrap/fixup_words.nula"],
            input=request,
            text=True,
            capture_output=True,
            cwd=ROOT,
            timeout=30,
            check=False,
        )
        self.assertEqual(process.returncode, 0, process.stderr)
        return process.stdout.strip()

    def test_jump_backward_twos_complement(self):
        self.assertEqual(self.patched("Jffff"), "50ffff00")

    def test_jump_forward_offset(self):
        self.assertEqual(self.patched("J0003"), "50000300")

    def test_conditional_jump_negative_offset(self):
        self.assertEqual(self.patched("F02ffff"), "5202ffff")

    def test_conditional_jump_register_and_positive_offset(self):
        self.assertEqual(self.patched("F7f0012"), "527f0012")

    def test_constu_index_and_destination(self):
        self.assertEqual(self.patched("C030102"), "07010203")

    def test_perform_index_and_destination(self):
        self.assertEqual(self.patched("P030102"), "90010203")

    def test_uppercase_hex_input(self):
        self.assertEqual(self.patched("F7F0012"), "527f0012")

    def test_invalid_digit_fails_closed(self):
        self.assertEqual(self.patched("Jzzzz"), "ERROR")

    def test_unknown_command_fails_closed(self):
        self.assertEqual(self.patched("Z0001"), "ERROR")

    def test_invalid_width_fails_closed(self):
        self.assertEqual(self.patched("J001"), "ERROR")


if __name__ == "__main__":
    unittest.main()
