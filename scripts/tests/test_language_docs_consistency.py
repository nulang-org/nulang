"""Regression checks for public language guides against implemented contracts.

Run: python3 -m unittest discover -s scripts/tests -p test_language_docs_consistency.py
These checks intentionally inspect the handwritten guides, not generated docs.
"""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
PITFALLS = ROOT / "docs" / "PITFALLS.md"
SAFETY = ROOT / "docs" / "src" / "content" / "docs" / "language" / "safety.md"
LIST = ROOT / "src" / "stdlib" / "list.nula"
SPEC = ROOT / "SPEC2.md"


def documented_list_groups(text: str) -> dict[str, set[str]]:
    section = text.split("### 17. List functions:", 1)[1].split("\n---\n", 1)[0]
    groups = {}
    for label in ("Polymorphic", "Int-only"):
        match = re.search(
            rf"^- \*\*{label}\*\*.*?(?=\n- \*\*|\n```|\Z)",
            section,
            re.MULTILINE | re.DOTALL,
        )
        if match is None:
            raise AssertionError(f"Missing {label} list-function group")
        groups[label] = set(re.findall(r"`([a-z][a-z_0-9]*)`", match.group()))
    return groups


class LanguageDocsConsistencyTests(unittest.TestCase):
    def test_list_function_groups_agree_with_source_signatures(self):
        docs = documented_list_groups(PITFALLS.read_text(encoding="utf-8"))
        declarations = {
            name: params
            for name, params in re.findall(
                r"^pub fn ([a-z][a-z_0-9]*)\(([^)]*)\)",
                LIST.read_text(encoding="utf-8"),
                flags=re.MULTILINE,
            )
        }
        self.assertTrue(declarations)
        self.assertFalse(docs["Polymorphic"] & docs["Int-only"])
        self.assertEqual(
            set(declarations), docs["Polymorphic"] | docs["Int-only"],
            "Every public list helper must be classified in the guide",
        )
        for name in docs["Polymorphic"]:
            with self.subTest(name=name):
                self.assertNotIn("[Int]", declarations[name])
        for name in docs["Int-only"]:
            with self.subTest(name=name):
                self.assertIn("[Int]", declarations[name])

    def test_effect_handler_resolution_is_documented_as_runtime(self):
        spec = SPEC.read_text(encoding="utf-8")
        guide = SAFETY.read_text(encoding="utf-8")
        section = guide.split("## Effect System", 1)[1].split("## Actor Isolation", 1)[0]
        self.assertIn("An unhandled effect is a runtime", spec)
        self.assertRegex(section.lower(), r"unhandled effect.{0,80}runtime")
        self.assertNotIn("unhandled effects are compile-time errors", section.lower())

    def test_sendability_guide_names_all_four_sendable_capabilities(self):
        guide = SAFETY.read_text(encoding="utf-8")
        self.assertIn("`iso`, `lineariso`, `val`, and `tag`", guide)


if __name__ == "__main__":
    unittest.main()
