#!/usr/bin/env python3
"""CI must build every *compatible* feature, excluding only runner marker."""
import importlib.util
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "compatible-cargo-features.py"


def load_selector():
    spec = importlib.util.spec_from_file_location("compatible_cargo_features", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CompatibleCargoFeatureTests(unittest.TestCase):
    def test_exactly_one_incompatible_marker_is_excluded(self):
        module = load_selector()
        features = {"default": [], "full": [], "tcp": [], "untrusted-runner": [],
                    "future-feature": [], "wasm-backend": []}
        self.assertEqual(module.compatible_features(features),
                         "default,full,future-feature,tcp,wasm-backend")

    def test_alias_to_untrusted_marker_fails_closed(self):
        module = load_selector()
        with self.assertRaises(ValueError):
            module.compatible_features({
                "untrusted-runner": [], "default": ["full"],
                "full": ["untrusted-runner"],
            })

    def test_missing_runner_marker_fails_closed(self):
        module = load_selector()
        with self.assertRaises(ValueError):
            module.compatible_features({"tcp": []})

    def test_current_cargo_metadata_has_no_unlisted_features(self):
        module = load_selector()
        all_features = module.cargo_features()
        compatible = set(module.compatible_features(all_features).split(","))
        self.assertEqual(compatible | {"untrusted-runner"}, set(all_features))


if __name__ == "__main__":
    unittest.main()
