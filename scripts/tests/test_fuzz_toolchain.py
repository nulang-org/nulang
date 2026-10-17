"""Keep nightly differential fuzz on the same Rust toolchain as primary CI."""

import re
import tomllib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class NightlyFuzzToolchainTests(unittest.TestCase):
    def test_nightly_fuzz_pins_exact_repository_rust_version(self):
        with (ROOT / "rust-toolchain.toml").open("rb") as source:
            expected = tomllib.load(source)["toolchain"]["channel"]

        workflow = (ROOT / ".github/workflows/fuzz-nightly.yml").read_text(
            encoding="utf-8"
        )
        install = workflow.split("      - name: Install Rust\n", 1)[1].split(
            "      - name: Cache cargo\n", 1
        )[0]
        self.assertIn("dtolnay/rust-toolchain@stable", install)
        match = re.search(
            r"(?m)^\s+toolchain:\s*['\"]?([\d.]+)['\"]?\s*$",
            install,
        )
        self.assertIsNotNone(match, "fuzz job must explicitly pin Rust version")
        self.assertEqual(match.group(1), expected)


if __name__ == "__main__":
    unittest.main()
