import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
RUST_WORKFLOW = ROOT / ".woodpecker" / "rust.yml"


class WoodpeckerRustConcurrencyTests(unittest.TestCase):
    def test_rust_validation_serializes_per_pull_request_not_repository_wide(self):
        source = RUST_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn(
            "concurrency:\n  limit: 1",
            source,
            "rust validation should retain one active run per concurrency group",
        )
        self.assertIn(
            "group: rust-validation-${CI_COMMIT_PULL_REQUEST}",
            source,
            "pull requests must not share one repository-wide rust-validation mutex",
        )
        self.assertNotIn(
            "group: rust-validation\n",
            source,
            "repository-wide serialization would queue unrelated pull requests behind each other",
        )


if __name__ == "__main__":
    unittest.main()
