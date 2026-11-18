"""Release Linux builds must use a supported fixed runner image.

Unpinned ubuntu-latest shifted to an image whose amd64/arm64 apt sources
resolved to different package versions. The resulting 38-downgrade plan
stopped the aarch64 job before Cargo could compile (issue #2658).
"""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
RELEASE = ROOT / ".github/workflows/release.yml"


class ReleaseRunnerStabilityTests(unittest.TestCase):
    def test_linux_release_matrix_pins_supported_lts_image(self):
        workflow = RELEASE.read_text()
        for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"):
            with self.subTest(target=target):
                match = re.search(
                    rf"(?m)^\s+- os: (\S+)\n\s+target: {re.escape(target)}$",
                    workflow,
                )
                self.assertIsNotNone(match, f"missing release matrix target {target}")
                self.assertEqual(match.group(1), "ubuntu-24.04")

    def test_arm64_retains_cross_target_smoke_and_does_not_force_downgrades(self):
        workflow = RELEASE.read_text()
        self.assertIn("aarch64-linux-gnu-gcc -x c -c", workflow)
        self.assertIn("cargo build --locked --release --target", workflow)
        self.assertNotIn("--allow-downgrades", workflow)


if __name__ == "__main__":
    unittest.main()
