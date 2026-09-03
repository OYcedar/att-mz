from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

from att_skill_tools import ToolError
from att_toolbox.coverage import coverage_projection
from att_toolbox.js import scan_javascript, static_code_targets
from att_toolbox.rpg import discover_game, parse_plugins
from att_toolbox.survey_io import verify_source_baseline
from att_toolbox.survey_sources import scan_game


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")


def _write_mz_game(root: Path, *, with_bootstrap: bool = False) -> None:
    (root / "data").mkdir(parents=True)
    (root / "js").mkdir(parents=True)
    (root / "js" / "rmmz_core.js").write_text("// core\n", encoding="utf-8")
    (root / "js" / "plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
    _write_json(
        root / "data" / "System.json",
        {
            "gameTitle": "标题",
            "currencyUnit": "",
            "terms": {"basic": [], "commands": [], "params": [], "messages": {}},
            "elements": [],
            "skillTypes": [],
            "weaponTypes": [],
            "armorTypes": [],
            "equipTypes": [],
        },
    )
    if with_bootstrap:
        _write_json(root / "package.json", {"name": "survey", "main": "index.html"})
        (root / "index.html").write_text(
            '<script src="js/rmmz_core.js"></script>\n<script src="js/main.js"></script>\n',
            encoding="utf-8",
        )


class PluginsEnvelopeTests(unittest.TestCase):
    def test_line_comment_declaration_decoy_does_not_hide_real_assignment(self) -> None:
        plugins = parse_plugins(
            '// var $plugins = [{"name":"Decoy"}];\r\n'
            "// generated\r\n"
            '  var $plugins = [{"name":"Real","status":true,"description":"","parameters":{}}];\r\n',
            "plugins.js",
        )

        self.assertEqual([plugin.name for plugin in plugins], ["Real"])

    def test_only_the_current_att_envelope_is_accepted(self) -> None:
        invalid = [
            "/* generated */\nvar $plugins = [];",
            "const $plugins = [];",
            "let $plugins = [];",
            "var $plugins = []",
            "var $plugins = []; trailing",
        ]

        for text in invalid:
            with self.subTest(text=text), self.assertRaises(ToolError):
                parse_plugins(text, "plugins.js")


class JavaScriptSourceTests(unittest.TestCase):
    def test_utf16_escape_pairs_become_scalars_and_lone_surrogates_stay_serializable(self) -> None:
        source = r'const pair = "\uD83D\uDE00"; const lone = "\uD83D";'
        scan = scan_javascript(source)

        self.assertEqual([literal.value for literal in scan.literals], ["😀", "�"])
        self.assertEqual(
            [source[literal.start : literal.end] for literal in scan.literals],
            [r'"\uD83D\uDE00"', r'"\uD83D"'],
        )
        json.dumps([literal.value for literal in scan.literals], ensure_ascii=False).encode("utf-8")

    def test_extensionless_static_targets_include_node_style_javascript_candidates(self) -> None:
        self.assertEqual(
            set(static_code_targets("./helper", "js/main.js")),
            {
                "js/helper.js",
                "js/helper.mjs",
                "js/helper/index.js",
                "js/helper/index.mjs",
                "helper.js",
                "helper.mjs",
                "helper/index.js",
                "helper/index.mjs",
            },
        )

    def test_html_main_and_its_extensionless_dependency_are_scanned(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            _write_mz_game(game, with_bootstrap=True)
            (game / "js" / "main.js").write_text(
                'require("./helper");\ndrawText("主入口正文");\n',
                encoding="utf-8",
            )
            (game / "js" / "Helper.js").write_text('drawText("依赖正文");\n', encoding="utf-8")

            locations = scan_game(game).locations

            self.assertTrue(
                any(
                    item["physical_file"] == "js/main.js" and item["source_text"] == "主入口正文"
                    for item in locations
                )
            )
            self.assertTrue(
                any(
                    item["physical_file"] == "js/Helper.js" and item["source_text"] == "依赖正文"
                    for item in locations
                )
            )

    def test_survey_serializes_utf16_escape_pairs_and_lone_surrogates(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            _write_mz_game(game, with_bootstrap=True)
            (game / "js" / "main.js").write_text(
                r'drawText("\uD83D\uDE00"); drawText("\uD83D");' + "\n",
                encoding="utf-8",
            )

            bundle = scan_game(game)

            self.assertTrue(any(item["source_text"] == "😀" for item in bundle.locations))
            self.assertTrue(any(item["source_text"] == "�" for item in bundle.locations))
            json.dumps(bundle.locations, ensure_ascii=False).encode("utf-8")

    def test_display_literal_does_not_activate_an_extensionless_namesake_script(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            _write_mz_game(game, with_bootstrap=True)
            (game / "js" / "main.js").write_text(
                'require("./helper"); drawText("menu");\n',
                encoding="utf-8",
            )
            (game / "js" / "helper.js").write_text('drawText("正确依赖");\n', encoding="utf-8")
            (game / "js" / "menu.js").write_text('drawText("不应扫描");\n', encoding="utf-8")

            locations = scan_game(game).locations

            self.assertTrue(any(item["source_text"] == "正确依赖" for item in locations))
            self.assertFalse(any(item["source_text"] == "不应扫描" for item in locations))

    def test_multiline_loader_literal_activates_its_script(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            _write_mz_game(game, with_bootstrap=True)
            (game / "js" / "main.js").write_text(
                'require(\n  "./helper"\n);\n',
                encoding="utf-8",
            )
            (game / "js" / "helper.js").write_text('drawText("多行依赖");\n', encoding="utf-8")

            locations = scan_game(game).locations

            self.assertTrue(any(item["source_text"] == "多行依赖" for item in locations))


class SurveySourceSelectionTests(unittest.TestCase):
    def test_uppercase_json_remains_a_generic_candidate_without_rules_projection(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            _write_mz_game(game)
            _write_json(game / "data" / "Custom.JSON", {"text": "外部正文"})

            fact = next(item for item in scan_game(game).locations if item["source_text"] == "外部正文")

            self.assertEqual(fact["physical_file"], "data/Custom.JSON")
            self.assertEqual(fact["generic_kind"], "json_string")
            self.assertNotIn("rule", fact)
            self.assertNotIn("expected_manual_id", fact)

    def test_adding_a_javascript_candidate_invalidates_the_scan_baseline(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            _write_mz_game(game, with_bootstrap=True)
            (game / "js" / "main.js").write_text('require("./late");\n', encoding="utf-8")
            bundle = scan_game(game)

            (game / "js" / "late.js").write_text('drawText("新增正文");\n', encoding="utf-8")

            with self.assertRaises(ToolError):
                verify_source_baseline(bundle.summary, bundle.source_baseline)

    def test_a_direct_data_and_js_content_root_is_a_complete_scan_scope(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "content"
            _write_mz_game(game)

            discovered = discover_game(game)

            self.assertEqual(discovered.content_root, game.resolve())
            self.assertEqual(discovered.game_root, game.resolve())
            self.assertEqual(scan_game(game).summary["game_root"], str(game.resolve()))

    def test_mv_baseline_ignores_javascript_outside_the_content_root(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            content = game / "www"
            (content / "data").mkdir(parents=True)
            (content / "js").mkdir(parents=True)
            (content / "js" / "rpg_core.js").write_text("// core\n", encoding="utf-8")
            (content / "js" / "plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
            _write_json(
                content / "data" / "System.json",
                {
                    "gameTitle": "标题",
                    "currencyUnit": "",
                    "terms": {"basic": [], "commands": [], "params": [], "messages": {}},
                    "elements": [],
                    "skillTypes": [],
                    "weaponTypes": [],
                    "armorTypes": [],
                    "equipTypes": [],
                },
            )
            (game / "launcher.js").write_text("// launcher\n", encoding="utf-8")

            bundle = scan_game(game)
            selected = bundle.source_baseline["selection"]
            self.assertIsInstance(selected, dict)
            self.assertNotIn("launcher.js", selected["paths"])

            (game / "later-launcher.js").write_text("// later launcher\n", encoding="utf-8")
            verify_source_baseline(bundle.summary, bundle.source_baseline)


class CoverageProjectionTests(unittest.TestCase):
    def test_unresolved_target_covers_all_members_when_detail_names_only_unsupported_subset(self) -> None:
        locations = [
            {
                "candidate_id": f"location-{number:06d}",
                "classification": "review",
                "review_group_id": "group-000001",
                "source": "data/Custom.json",
                "source_text": text,
                "physical_file": "data/Custom.json",
            }
            for number, text in ((1, "普通正文"), (2, "含\r回车"))
        ]
        coverage = {
            "complete": False,
            "engine": "mz",
            "builtin_candidate_ids": [],
            "resource_reference_candidate_ids": [],
            "structural_whitespace_candidate_ids": [],
            "dispositions": [],
            "unresolved": [
                {
                    "target": "group:group-000001",
                    "reason": "generic_roundtrip_not_supported",
                    "candidate_ids": ["location-000002"],
                }
            ],
            "missing_targets": [],
            "expected_ownership": [],
            "unit_projection": [],
            "counts": {
                "locations": 2,
                "review_groups": 1,
                "decisions": 1,
                "rules": 0,
                "generic_groups": 0,
                "unresolved": 1,
            },
        }
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "coverage.json"
            _write_json(path, coverage)

            _projection, _generic, complete, _plans = coverage_projection(
                path,
                {"engine": "mz"},
                locations,
                [{"group_id": "group-000001"}],
                [],
            )

            self.assertFalse(complete)


class MzSpeakerTests(unittest.TestCase):
    def test_falsy_speakers_follow_mz_normalization_and_speaker_only_dialogue_is_kept(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            _write_mz_game(game)
            commands: list[dict[str, object]] = []
            for falsy in [None, False, 0, 0.0, ""]:
                commands.extend(
                    [
                        {"code": 101, "parameters": ["", 0, 0, 2, falsy]},
                        {"code": 401, "parameters": ["正文"]},
                    ]
                )
            commands.extend(
                [
                    {"code": 101, "parameters": ["", 0, 0, 2, "名字"]},
                    {"code": 0, "parameters": []},
                ]
            )
            _write_json(game / "data" / "CommonEvents.json", [None, {"list": commands}])

            locations = scan_game(game).locations

            self.assertEqual(
                sum(
                    item["source_text"] == "正文" and isinstance(item.get("expected_manual_id"), str)
                    for item in locations
                ),
                5,
            )
            speakers = [
                item
                for item in locations
                if item.get("expected_manual_id") and str(item["expected_manual_id"]).endswith(":speaker")
            ]
            self.assertEqual([item["source_text"] for item in speakers], ["名字"])

    def test_speaker_only_truthy_non_string_is_rejected(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "game"
            _write_mz_game(game)
            _write_json(
                game / "data" / "CommonEvents.json",
                [None, {"list": [{"code": 101, "parameters": ["", 0, 0, 2, True]}]}],
            )

            with self.assertRaises(ToolError):
                scan_game(game)


if __name__ == "__main__":
    unittest.main()
