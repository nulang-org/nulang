#!/usr/bin/env python3
"""Ensure every Cargo feature is explicitly fenced from the hosted runner."""
import json
import re
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LIB = ROOT / "src/lib.rs"


def cargo_features():
    # Cargo metadata includes implicit optional-dependency features that a
    # TOML [features] scan could miss when dependencies are added.
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--locked", "--offline",
         "--format-version", "1"], cwd=ROOT, capture_output=True,
        text=True, check=True,
    )
    package = next(pkg for pkg in json.loads(result.stdout)["packages"]
                   if pkg["name"] == "nulang")
    return set(package["features"])


def unfenced_features(feature_names, source):
    marker = 'compile_error!("untrusted-runner forbids host-integrating features'
    prefix, sep, _ = source.partition(marker)
    if not sep:
        raise AssertionError("untrusted-runner compile_error guard is missing")
    block = prefix.rsplit("#[cfg(all(", 1)[-1]
    if block == prefix:
        raise AssertionError("untrusted-runner cfg guard is missing")
    guarded = set(re.findall(r'feature\s*=\s*"([^"]+)"', block))
    if 'untrusted-runner' not in guarded:
        raise AssertionError("untrusted-runner marker is missing from cfg guard")
    return set(feature_names) - guarded


class UntrustedRunnerFeatureCoverage(unittest.TestCase):
    def test_all_current_cargo_features_are_explicitly_fenced(self):
        features = cargo_features()
        source = LIB.read_text()
        self.assertEqual(unfenced_features(features, source), set())

    def test_new_unsafe_feature_fails_closed_until_guard_is_updated(self):
        features = cargo_features()
        source = LIB.read_text()
        features.add("future-host-network-api")
        self.assertIn("future-host-network-api", unfenced_features(features, source))

    def test_removing_guard_is_never_silent(self):
        with self.assertRaises(AssertionError):
            unfenced_features(["untrusted-runner", "tcp"], "pub mod vm;")


if __name__ == "__main__":
    unittest.main()
