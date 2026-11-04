"""Prove local SQLite does not force the legacy HTTP/2 remote client into builds."""

import pathlib
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


def dependencies(*features: str) -> set[str]:
    command = ["cargo", "tree", "--locked", "-e", "normal", "--prefix", "none"]
    if features:
        command.extend(["--no-default-features", "--features", ",".join(features)])
    return set(subprocess.check_output(
        command,
        cwd=ROOT,
        text=True,
        timeout=120,
    ).splitlines())


class SqliteFeatureBoundary(unittest.TestCase):
    def test_local_sqlite_excludes_legacy_h2(self):
        local = dependencies("sqlite")
        self.assertIn("libsql v0.10.0-pre.4", local)
        self.assertNotIn("h2 v0.3.27", local)
        self.assertNotIn("hyper v0.14.32", local)

    def test_default_cli_profile_excludes_legacy_h2(self):
        default = dependencies()
        self.assertNotIn("h2 v0.3.27", default)
        self.assertNotIn("hyper v0.14.32", default)

    def test_remote_opt_in_retains_legacy_adapter_until_migration(self):
        remote = dependencies("sqlite-remote")
        self.assertIn("libsql v0.10.0-pre.4", remote)
        self.assertIn("h2 v0.3.27", remote)
        self.assertIn("hyper v0.14.32", remote)

    def test_hosted_ci_exercises_both_sqlite_features(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("sqlite-boundary:", workflow)
        self.assertIn("--no-default-features --features sqlite --test sqlite_feature_boundary", workflow)
        self.assertIn("--no-default-features --features sqlite-remote --lib runtime::libsql_tls::tests", workflow)


if __name__ == "__main__":
    unittest.main()
