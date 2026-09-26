from __future__ import annotations

import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

import inspect_nwjs_runtime as runtime
from att_skill_tools import ToolError
from att_toolbox.font_references import build_font_plan
from att_toolbox.nwjs import CdpTarget, unique_content_target
from att_toolbox.rpg import discover_game
from att_toolbox.survey_sources import scan_game


def _game(root: Path, *, engine: str, root_package: bool, root_html: bool = False) -> tuple[Path, Path]:
    content = root / "www" if engine == "mv" else root
    (content / "data").mkdir(parents=True)
    (content / "js").mkdir()
    (content / "fonts").mkdir()
    (content / "data" / "System.json").write_text(
        json.dumps(
            {
                "gameTitle": "",
                "currencyUnit": "",
                "terms": {"basic": [], "commands": [], "params": [], "messages": {}},
                "elements": [],
                "skillTypes": [],
                "weaponTypes": [],
                "armorTypes": [],
                "equipTypes": [],
            }
        ),
        encoding="utf-8",
    )
    core = "rpg_core.js" if engine == "mv" else "rmmz_core.js"
    (content / "js" / core).write_text("// core\n", encoding="utf-8")
    (content / "js" / "plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
    (content / "fonts" / "Old.ttf").write_bytes(b"old-font")
    (content / "js" / "font-loader.js").write_text(
        'Graphics.loadFont("RuntimeFont", "fonts/Old.ttf");\ndrawText("Visible label");\n',
        encoding="utf-8",
    )
    html = (root if root_html else content) / "launch page.html"
    script_source = (content / "js" / "font-loader.js").relative_to(html.parent).as_posix()
    html.write_text(f'<script src="{script_source}"></script>\n', encoding="utf-8")
    package_root = root if root_package else content
    main = html.relative_to(package_root).as_posix().replace(" ", "%20")
    (package_root / "package.json").write_text(json.dumps({"main": main}), encoding="utf-8")
    return content, html


class NwjsEntryConsumersTests(unittest.TestCase):
    def test_font_survey_and_runtime_follow_root_and_content_package_entries(self) -> None:
        selected = ROOT / "skills" / "translate-with-att" / "assets" / "fonts" / "LXGWWenKaiGB-Regular.ttf"
        layouts = (("mv", True, False), ("mv", False, False), ("mz", True, False), ("mv", True, True))
        for engine, root_package, root_html in layouts:
            with (
                self.subTest(engine=engine, root_package=root_package, root_html=root_html),
                tempfile.TemporaryDirectory() as tmp,
            ):
                root = Path(tmp)
                content, html = _game(root, engine=engine, root_package=root_package, root_html=root_html)
                plan = build_font_plan(game_root=root, content_root=content, selected_font=selected)
                relative_loader = (content / "js" / "font-loader.js").relative_to(root).as_posix()
                self.assertEqual(
                    {mutation.relative_path for mutation in plan.mutations},
                    {
                        relative_loader,
                        (content / "fonts" / selected.name).relative_to(root).as_posix(),
                    },
                )
                self.assertFalse(plan.reviews)
                entry = runtime._runtime_entry(discover_game(root))
                self.assertEqual(entry, html)
                target = CdpTarget("game", entry.as_uri(), "ws://127.0.0.1/devtools/page/game")
                self.assertEqual(
                    unique_content_target((target,), expected_game_root=root, expected_entry=entry),
                    target,
                )
                self.assertTrue(
                    any(
                        location["physical_file"] == relative_loader
                        and location["source_text"] == "Visible label"
                        for location in scan_game(root).locations
                    )
                )

    def test_font_and_runtime_use_the_executable_root_when_both_packages_exist(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            content, content_html = _game(root, engine="mv", root_package=False)
            (root / "Game.exe").write_bytes(b"")
            root_html = root / "root.html"
            root_html.write_text('<script src="www/js/root-loader.js"></script>', encoding="utf-8")
            (root / "package.json").write_text('{"main":"root.html"}', encoding="utf-8")
            (content / "js" / "root-loader.js").write_text(
                'Graphics.loadFont("RootFont", "fonts/Old.ttf");', encoding="utf-8"
            )
            game = discover_game(content)
            self.assertEqual(game.supplied_root, content)
            self.assertEqual(runtime._runtime_entry(game), root_html)
            selected = (
                ROOT / "skills" / "translate-with-att" / "assets" / "fonts" / "LXGWWenKaiGB-Regular.ttf"
            )
            plan = build_font_plan(
                game_root=game.game_root, content_root=game.content_root, selected_font=selected
            )
            changed = {mutation.relative_path for mutation in plan.mutations}
            self.assertIn("www/js/root-loader.js", changed)
            self.assertNotIn("www/js/font-loader.js", changed)
            survey = scan_game(content)
            self.assertTrue(
                any(
                    location["physical_file"] == "www/js/font-loader.js"
                    and location["source_text"] == "Visible label"
                    for location in survey.locations
                )
            )
            self.assertIn(
                {
                    "path": content_html.relative_to(root).as_posix(),
                    "bytes": len(content_html.read_bytes()),
                    "sha256": hashlib.sha256(content_html.read_bytes()).hexdigest(),
                },
                survey.source_baseline["files"],
            )

    def test_runtime_entry_stays_within_the_supplied_game(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            parent = Path(tmp)
            root = parent / "game"
            _game(root, engine="mv", root_package=True)
            (parent / "outside.html").write_text("<html></html>", encoding="utf-8")
            for main in ("../outside.html", "https://example.test/index.html"):
                with self.subTest(main=main):
                    (root / "package.json").write_text(json.dumps({"main": main}), encoding="utf-8")
                    with self.assertRaises(ToolError):
                        runtime._runtime_entry(discover_game(root))


if __name__ == "__main__":
    unittest.main()
