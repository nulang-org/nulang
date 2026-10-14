"""Keep human and agent pre-merge entrypoints on one CI-local contract."""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]


def recipe_body(justfile: str, name: str) -> str:
    match = re.search(
        rf"(?m)^{re.escape(name)}:\s*\n((?:^[ \t]+.*\n|^\n)*)",
        justfile,
    )
    if match is None:
        raise AssertionError(f"missing just recipe: {name}")
    return match.group(1)


class LocalValidationContractTests(unittest.TestCase):
    def test_just_and_mise_check_share_canonical_script(self):
        justfile = (ROOT / "Justfile").read_text(encoding="utf-8")
        mise = (ROOT / "mise.toml").read_text(encoding="utf-8")
        self.assertIn("bash scripts/ci-local.sh", recipe_body(justfile, "check"))
        self.assertIn('run = "bash scripts/ci-local.sh"', mise)

    def test_full_check_is_explicit_and_shared(self):
        justfile = (ROOT / "Justfile").read_text(encoding="utf-8")
        mise = (ROOT / "mise.toml").read_text(encoding="utf-8")
        self.assertIn("bash scripts/ci-local.sh --full", recipe_body(justfile, "check-full"))
        self.assertIn('run = "bash scripts/ci-local.sh --full"', mise)

    def test_verifier_avoids_duplicate_test_suite(self):
        script = (ROOT / "scripts/ci-local.sh").read_text(encoding="utf-8")
        self.assertIn("run cargo test --locked", script)
        self.assertIn("run python3 scripts/verify_implementation.py --skip-tests", script)
        self.assertNotRegex(
            script,
            r"(?m)^run python3 scripts/verify_implementation.py$",
        )


if __name__ == "__main__":
    unittest.main()
