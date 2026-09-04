from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

import rpg_maker_survey
from att_skill_tools import ToolError
from att_toolbox.coverage import coverage_projection
from att_toolbox.generic_mapping import (
    generic_recipe,
    validate_generic_evidence,
    validate_generic_group_placement,
)


class SurveyArtifactBindingTests(unittest.TestCase):
    def test_decisions_bind_game_and_natural_members(self) -> None:
        groups = [{"group_id": "group-000001", "candidate_ids": ["candidate-000001"]}]
        locations = [
            {
                "candidate_id": "candidate-000001",
                "classification": "review",
                "review_group_id": "group-000001",
                "source": "extras/text.txt",
                "location": "extras/text.txt:line1",
                "source_text": "Visible",
            }
        ]
        row = rpg_maker_survey._ownership_decision_template(groups, locations, "C:/game")[0]
        self.assertEqual(row["game_root"], "C:/game")
        self.assertEqual(row["members"], locations)

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "decisions.jsonl"
            wrong = dict(row)
            wrong["game_root"] = "C:/another-game"
            path.write_text(json.dumps(wrong) + "\n", encoding="utf-8")
            with self.assertRaises(ToolError):
                rpg_maker_survey._decision_rows(path, groups, locations, "C:/game")

            changed_locations = [{**locations[0], "roles": ["protocol"]}]
            path.write_text(json.dumps(row) + "\n", encoding="utf-8")
            with self.assertRaises(ToolError):
                rpg_maker_survey._decision_rows(path, groups, changed_locations, "C:/game")

    def test_coverage_requires_current_complete_schema(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "coverage.json"
            path.write_text('{"engine":"mz","complete":true}', encoding="utf-8")
            with self.assertRaises(ToolError):
                coverage_projection(path, {"engine": "mz"}, [], [], [])
            with self.assertRaises(ToolError):
                coverage_projection(path, {"engine": []}, [], [], [])

            path.write_text(
                json.dumps(
                    {
                        "engine": "mz",
                        "complete": True,
                        "counts": {
                            "locations": 1,
                            "review_groups": 0,
                            "rules": 0,
                            "generic_groups": 0,
                            "unresolved": 0,
                        },
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(ToolError):
                coverage_projection(
                    path,
                    {"engine": "mz"},
                    [{"candidate_id": "location-000001", "classification": "unknown"}],
                    [],
                    [],
                )

    def test_generic_recipes_reject_invalid_text_and_support_json_paths(self) -> None:
        base = {
            "candidate_id": "candidate-000001",
            "physical_file": "extras/config.json",
            "generic_kind": "json_string",
            "generic_locator": {"path": ["window", "title"], "decode_positions": [1, 2]},
        }
        self.assertIsNone(generic_recipe({**base, "source_text": "bad\rtext"}))
        recipe = generic_recipe({**base, "source_text": "Visible"})
        self.assertEqual(recipe["path"] if recipe is not None else None, ["window", "title"])
        self.assertIsNone(generic_recipe({**base, "generic_kind": [], "source_text": "x"}))

    def test_shared_generic_group_keeps_its_source_and_kind(self) -> None:
        candidate_ids = ["location-000001", "location-000002"]
        evidence = validate_generic_evidence(
            {
                "exact_location": "confirmed",
                "active_runtime_consumer": "confirmed",
                "player_visible_non_image_text": "confirmed",
                "builtin_not_owner": "confirmed",
                "rules_cannot_map_reversibly": "confirmed",
                "extract_group_unit_write_back_mapping": {
                    "groups": [{"id": "shared", "kind": "entry", "candidate_ids": candidate_ids}]
                },
                "unique_owner": "confirmed",
            },
            candidate_ids,
            "test evidence",
        )
        same_file = {candidate_id: {"physical_file": "extras/data.json"} for candidate_id in candidate_ids}
        placements: dict[str, tuple[str, str]] = {}
        validate_generic_group_placement(evidence, same_file, placements, "test evidence")
        validate_generic_group_placement(evidence, same_file, placements, "another ownership decision")
        changed_kind = json.loads(json.dumps(evidence))
        changed_kind["extract_group_unit_write_back_mapping"]["groups"][0]["kind"] = "different"
        with self.assertRaises(ToolError):
            validate_generic_group_placement(changed_kind, same_file, placements, "test evidence")
        with self.assertRaises(ToolError):
            validate_generic_group_placement(
                evidence,
                {
                    "location-000001": {"physical_file": "extras/a.json"},
                    "location-000002": {"physical_file": "extras/b.json"},
                },
                {},
                "test evidence",
            )

    def test_generic_group_identity_allows_nonempty_whitespace_like_att_jsonl(self) -> None:
        evidence = validate_generic_evidence(
            {
                "exact_location": "confirmed",
                "active_runtime_consumer": "confirmed",
                "player_visible_non_image_text": "confirmed",
                "builtin_not_owner": "confirmed",
                "rules_cannot_map_reversibly": "confirmed",
                "extract_group_unit_write_back_mapping": {
                    "groups": [{"id": " ", "kind": "\t", "candidate_ids": ["location-000001"]}]
                },
                "unique_owner": "confirmed",
            },
            ["location-000001"],
            "test evidence",
        )

        group = evidence["extract_group_unit_write_back_mapping"]["groups"][0]
        self.assertEqual(group["id"], " ")
        self.assertEqual(group["kind"], "\t")


if __name__ == "__main__":
    unittest.main()
