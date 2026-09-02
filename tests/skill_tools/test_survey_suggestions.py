from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

from att_toolbox.survey_suggestions import capture_pattern, rule_proposal


class SurveySuggestionTests(unittest.TestCase):
    def test_control_structure_is_not_split_as_punctuation_wrapper(self) -> None:
        for value in [
            r"\PS[1]" + "\n……。",
            r"\N<1>" + "\n……。",
            r"\G金币。",
            "%1获得了物品。",
        ]:
            with self.subTest(value=value):
                self.assertEqual(capture_pattern(value), (None, None))
                self.assertEqual(
                    rule_proposal({"file": "Custom.json", "path": []}, value),
                    ({"file": "Custom.json", "path": []}, []),
                )

    def test_normal_punctuation_wrappers_remain_reversible(self) -> None:
        for value, expected in [
            ("「台词」", r"\A「(?<text>(?s:.+?))」\z"),
            ("『台词』", r"\A『(?<text>(?s:.+?))』\z"),
        ]:
            with self.subTest(value=value):
                pattern, evidence = capture_pattern(value)
                self.assertEqual(pattern, expected)
                self.assertEqual(evidence["analysis_status"] if evidence is not None else None, "confirmed")


if __name__ == "__main__":
    unittest.main()
