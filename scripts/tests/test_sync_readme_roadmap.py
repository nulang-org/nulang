"""Offline unit coverage for the README roadmap renderer and marker safety."""

import importlib.util
import unittest
from pathlib import Path

module_path = Path(__file__).parents[1] / "sync_readme_roadmap.py"
spec = importlib.util.spec_from_file_location("readme_roadmap", module_path)
roadmap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(roadmap)


class RoadmapTests(unittest.TestCase):
    def test_parse_curated_sections_and_dedupe_references(self):
        body = """## P0 — semantic correctness stack
- #12 / #13 — resolve first
- #12 — duplicate reference
## Existing identity/authority foundations
- #99 — historical, not a current roadmap track
## Protocol and delivery evolution
- #22 — protocol
"""
        selected = roadmap.tracked_issues(body)
        self.assertEqual(selected["P0 — semantic correctness stack"], [12, 13])
        self.assertEqual(selected["Protocol and delivery evolution"], [22])
        self.assertNotIn(99, {item for refs in selected.values() for item in refs})

    def test_refuses_missing_tracker_references(self):
        with self.assertRaisesRegex(ValueError, "no recognized"):
            roadmap.tracked_issues("## Overview\n- #99")

    def test_render_does_not_equate_closed_with_completed(self):
        selected = {heading: [] for heading, _ in roadmap.TRACKS}
        selected[roadmap.TRACKS[0][0]] = [12, 13]
        output = roadmap.render(selected, {12: "open", 13: "closed"})
        self.assertIn("| Semantic correctness (P0) | 1 / 2 |", output)
        self.assertIn("[#12]", output)
        self.assertNotIn("[#13]", output)
        self.assertIn("not verified feature completion", output)

    def test_replaces_only_marker_block_idempotently(self):
        original = "# Header\n\nIntro\n<!-- roadmap:start -->\nold\n<!-- roadmap:end -->\n\n# Footer\n"
        first = roadmap.replace_block(original, "new\ntext")
        self.assertTrue(first.startswith("# Header\n\nIntro\n"))
        self.assertTrue(first.endswith("\n\n# Footer\n"))
        self.assertEqual(roadmap.replace_block(first, "new\ntext"), first)

    def test_refuses_duplicate_or_reversed_markers(self):
        for text in ("README without markers", "<!-- roadmap:start --><!-- roadmap:start --><!-- roadmap:end -->", "<!-- roadmap:end --><!-- roadmap:start -->"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                roadmap.replace_block(text, "generated")


if __name__ == "__main__":
    unittest.main()
