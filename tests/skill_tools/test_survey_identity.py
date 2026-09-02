from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

from att_toolbox.rpg import PluginInfo
from att_toolbox.survey_identity import rule_manual_id


class SurveyIdentityTests(unittest.TestCase):
    def test_rule_ids_match_att_source_classification_and_numbering(self) -> None:
        cases = [
            (
                "standard database",
                (2, "customName"),
                "Actors.json",
                None,
                False,
                'Actors.json:2:"[\\"customName\\"].text[0]"',
            ),
            (
                "common event command",
                (1, "list", 0, "parameters", 3, "text"),
                "CommonEvents.json",
                (1, "list", 0),
                False,
                'CommonEvents.json:1:list:1:"[\\"parameters\\"][3][\\"text\\"].text[0]"',
            ),
            (
                "troop event command",
                (2, "pages", 0, "list", 0, "parameters", 0, "text"),
                "Troops.json",
                (2, "pages", 0, "list", 0),
                False,
                'Troops.json:2:pages:1:list:1:"[\\"parameters\\"][0][\\"text\\"].text[0]"',
            ),
            (
                "canonical map command",
                ("events", 1, "pages", 0, "list", 0, "parameters", 3, "text"),
                "Map001.json",
                ("events", 1, "pages", 0, "list", 0),
                False,
                'Map001.json:event1:page1:command1:"[\\"parameters\\"][3][\\"text\\"].text[0]"',
            ),
            (
                "canonical map fallback",
                ("events", 2, "custom", 0, "name"),
                "Map001.json",
                None,
                False,
                'Map001.json:events:2:custom:1:"[\\"name\\"].text[0]"',
            ),
            (
                "custom data file",
                (2, "entries", 0, "name"),
                "QuestData.json",
                None,
                False,
                'QuestData.json:3:entries:1:"[\\"name\\"].text[0]"',
            ),
            (
                "noncanonical map file",
                ("events", 1, "pages", 0, "list", 0, "parameters", 3, "text"),
                "Map01.json",
                ("events", 1, "pages", 0, "list", 0),
                False,
                'Map01.json:events:2:pages:1:list:1:"[\\"parameters\\"][3][\\"text\\"].text[0]"',
            ),
            (
                "zero map file",
                ("events", 1, "pages", 0, "list", 0, "parameters", 3, "text"),
                "Map000.json",
                ("events", 1, "pages", 0, "list", 0),
                False,
                'Map000.json:events:2:pages:1:list:1:"[\\"parameters\\"][3][\\"text\\"].text[0]"',
            ),
        ]

        for name, path, source_file, command_steps, path_has_index, expected in cases:
            with self.subTest(name=name):
                self.assertEqual(
                    rule_manual_id(
                        path,
                        (),
                        source_file=source_file,
                        command_group_steps=command_steps,
                        command_path_has_index=path_has_index,
                    ),
                    expected,
                )

    def test_plugin_rule_id_is_unchanged(self) -> None:
        plugin = PluginInfo(
            name="MessagePlus",
            status=True,
            description="",
            parameters={},
            index=1,
        )

        self.assertEqual(
            rule_manual_id(
                ("Layout", "speaker"),
                (),
                source_file="plugins.js",
                plugin=plugin,
            ),
            'plugins.js:plugin2:MessagePlus:Layout:"[\\"speaker\\"].text[0]"',
        )


if __name__ == "__main__":
    unittest.main()
