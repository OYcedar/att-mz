from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

from att_toolbox.survey_projection import project_builtin_units
from att_toolbox.survey_sources import scan_game


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")


def _write_game(
    root: Path,
    engine: str,
    *,
    game_title: str = "原题",
    package_title: str = "原题",
    html_title: str = "原题",
    root_package: bool = True,
) -> tuple[Path, str, str]:
    content = root if engine == "mz" else root / "www"
    (content / "data").mkdir(parents=True)
    (content / "js").mkdir(parents=True)
    (root / "Game.exe").write_bytes(b"")
    core = "rmmz_core.js" if engine == "mz" else "rpg_core.js"
    (content / "js" / core).write_text("// core\n", encoding="utf-8")
    (content / "js" / "plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
    _write_json(
        content / "data" / "System.json",
        {
            "gameTitle": game_title,
            "currencyUnit": "",
            "terms": {"basic": [], "commands": [], "params": [], "messages": {}},
            "elements": [],
            "skillTypes": [],
            "weaponTypes": [],
            "armorTypes": [],
            "equipTypes": [],
        },
    )

    if root_package:
        package = root / "package.json"
        main_relative = "launch.html" if engine == "mz" else "www/launch.html"
        html = root / main_relative
    else:
        package = content / "package.json"
        main_relative = "launch.html"
        html = content / main_relative
    _write_json(
        package,
        {"name": "survey", "main": main_relative, "window": {"title": package_title}},
    )
    html.write_text(
        f'<!doctype html>\n<head>\n<title>{html_title}</title>\n<script src="js/{core}"></script>\n</head>\n',
        encoding="utf-8",
    )
    decoy = content / "index.html"
    if decoy != html:
        decoy.write_text("<title>备用入口</title>\n", encoding="utf-8")
    return package, package.relative_to(root).as_posix(), html.relative_to(root).as_posix()


def _derived(locations: list[dict[str, object]]) -> list[dict[str, object]]:
    return [
        item
        for item in locations
        if any(
            isinstance(evidence, dict) and evidence.get("kind") == "builtin_derived_consumer"
            for evidence in item["consumer_evidence"]  # type: ignore[union-attr]
        )
    ]


class SurveyBootstrapTitleTests(unittest.TestCase):
    def test_root_bootstrap_equal_titles_are_builtin_derived_consumers_without_units(self) -> None:
        for engine in ["mv", "mz"]:
            with self.subTest(engine=engine), TemporaryDirectory() as temporary:
                game = Path(temporary) / engine
                _package, package_relative, html_relative = _write_game(game, engine)

                locations = list(scan_game(game).locations)
                consumers = _derived(locations)

                self.assertEqual(
                    {item["physical_file"] for item in consumers},
                    {package_relative, html_relative},
                )
                self.assertEqual({item["classification"] for item in consumers}, {"builtin"})
                self.assertTrue(
                    all("expected_manual_id" not in item and "generic_kind" not in item for item in consumers)
                )
                self.assertEqual(
                    {
                        evidence["consumer"]
                        for item in consumers
                        for evidence in item["consumer_evidence"]
                        if evidence.get("kind") == "builtin_derived_consumer"
                    },
                    {"package.window.title", "package.main.html.title"},
                )
                self.assertTrue(
                    all(
                        evidence.get("owner_manual_id") == "System.json:gameTitle"
                        for item in consumers
                        for evidence in item["consumer_evidence"]
                        if evidence.get("kind") == "builtin_derived_consumer"
                    )
                )
                projected = project_builtin_units(locations)
                self.assertEqual(
                    sum(item["manual_id"] == "System.json:gameTitle" for item in projected),
                    1,
                )

    def test_empty_and_different_titles_keep_existing_external_classification(self) -> None:
        for package_title, html_title, package_classification in [
            ("", "自定义题", "structural_whitespace"),
            ("自定义题", "", "review"),
        ]:
            with (
                self.subTest(package_title=package_title, html_title=html_title),
                TemporaryDirectory() as temporary,
            ):
                game = Path(temporary) / "mz"
                _package, package_relative, html_relative = _write_game(
                    game,
                    "mz",
                    package_title=package_title,
                    html_title=html_title,
                )

                locations = list(scan_game(game).locations)

                self.assertFalse(_derived(locations))
                package_fact = next(
                    item
                    for item in locations
                    if item["physical_file"] == package_relative and item["json_path"] == ["window", "title"]
                )
                self.assertEqual(package_fact["classification"], package_classification)
                self.assertEqual(package_fact["generic_kind"], "json_string")
                html_fact = next(
                    item
                    for item in locations
                    if item["physical_file"] == html_relative
                    and item["source_text"] == f"<title>{html_title}</title>"
                )
                self.assertEqual(html_fact["classification"], "review")
                self.assertEqual(html_fact["generic_kind"], "plain_text_line")

    def test_mv_content_package_remains_external_without_root_package(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "mv"
            _package, package_relative, html_relative = _write_game(game, "mv", root_package=False)

            locations = list(scan_game(game).locations)

            self.assertFalse(_derived(locations))
            self.assertTrue(
                any(
                    item["physical_file"] == package_relative
                    and item["json_path"] == ["window", "title"]
                    and item["classification"] == "review"
                    for item in locations
                )
            )
            self.assertTrue(
                any(
                    item["physical_file"] == html_relative
                    and item["source_text"] == "<title>原题</title>"
                    and item["classification"] == "review"
                    for item in locations
                )
            )

    def test_derived_main_uses_the_rust_safe_relative_html_contract(self) -> None:
        for main in [
            "www/../launch.html",
            "../launch.html",
            "launch.htm",
            "launch.HTML",
            "launch.html?debug=1",
            "www\\launch.html",
        ]:
            with self.subTest(main=main), TemporaryDirectory() as temporary:
                game = Path(temporary) / "mz"
                package, _package_relative, _html_relative = _write_game(game, "mz")
                _write_json(
                    package,
                    {"name": "survey", "main": main, "window": {"title": "原题"}},
                )

                self.assertFalse(_derived(list(scan_game(game).locations)))

    def test_html_title_is_derived_while_other_content_on_the_line_remains_review(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "mz"
            _package, _package_relative, html_relative = _write_game(game, "mz")
            (game / html_relative).write_text(
                '<head><title>原题</title><span title="別題">別題</span></head>\n',
                encoding="utf-8",
            )

            locations = list(scan_game(game).locations)
            html_consumers = [item for item in _derived(locations) if item["physical_file"] == html_relative]
            self.assertEqual(len(html_consumers), 1)
            self.assertTrue(
                any(
                    item["physical_file"] == html_relative
                    and item["classification"] == "review"
                    and "別題" in str(item["source_text"])
                    for item in locations
                )
            )

    def test_html_title_ignores_comment_script_and_style_decoys(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "mz"
            _package, _package_relative, html_relative = _write_game(game, "mz")
            decoy_lines = {
                "<!-- <title>原题</title> -->",
                '<script>const decoy = "<title>原题</title>";</script>',
                '<style>.decoy::after { content: "<title>原题</title>"; }</style>',
            }
            (game / html_relative).write_text(
                "<head>\n"
                "<!-- <title>原题</title> -->\n"
                '<script>const decoy = "<title>原题</title>";</script>\n'
                '<style>.decoy::after { content: "<title>原题</title>"; }</style>\n'
                "<title>原题</title>\n"
                "</head>\n",
                encoding="utf-8",
            )

            locations = list(scan_game(game).locations)
            html_consumers = [item for item in _derived(locations) if item["physical_file"] == html_relative]

            self.assertEqual(len(html_consumers), 1)
            self.assertEqual(html_consumers[0]["source_text"], "原题")
            self.assertEqual(
                {
                    item["source_text"]
                    for item in locations
                    if item["physical_file"] == html_relative
                    and item["classification"] == "review"
                    and item["source_text"] in decoy_lines
                },
                decoy_lines,
            )
            self.assertFalse(
                any(
                    item["physical_file"] == html_relative
                    and item.get("generic_kind") == "plain_text_line"
                    and item["source_text"] == "<title>原题</title>"
                    for item in locations
                )
            )

    def test_html_title_decoys_without_an_element_remain_review(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "mz"
            _package, _package_relative, html_relative = _write_game(game, "mz")
            decoy_lines = [
                "<!-- <title>原题</title> -->",
                '<script>const decoy = "<title>原题</title>";</script>',
                '<style>.decoy::after { content: "<title>原题</title>"; }</style>',
            ]
            (game / html_relative).write_text("\n".join(decoy_lines) + "\n", encoding="utf-8")

            locations = list(scan_game(game).locations)

            self.assertFalse(any(item["physical_file"] == html_relative for item in _derived(locations)))
            self.assertEqual(
                [
                    item["source_text"]
                    for item in locations
                    if item["physical_file"] == html_relative and item["classification"] == "review"
                ],
                decoy_lines,
            )

    def test_html_title_ignores_inert_and_foreign_title_decoys(self) -> None:
        decoy_lines = [
            "<textarea><title>原题</title></textarea>",
            "<template><title>原题</title></template>",
            "<template><template/><title>原题</title></template><title>原题</title></template>",
            "<svg><title>原题</title></svg>",
            "<math><title>原题</title></math>",
        ]
        for include_actual in [False, True]:
            with self.subTest(include_actual=include_actual), TemporaryDirectory() as temporary:
                game = Path(temporary) / "mz"
                _package, _package_relative, html_relative = _write_game(game, "mz")
                lines = [*decoy_lines]
                if include_actual:
                    lines.append("<title>原题</title>")
                (game / html_relative).write_text("\n".join(lines) + "\n", encoding="utf-8")

                locations = list(scan_game(game).locations)
                html_consumers = [
                    item for item in _derived(locations) if item["physical_file"] == html_relative
                ]

                self.assertEqual(len(html_consumers), int(include_actual))
                self.assertEqual(
                    {
                        item["source_text"]
                        for item in locations
                        if item["physical_file"] == html_relative
                        and item["classification"] == "review"
                        and item["source_text"] in decoy_lines
                    },
                    set(decoy_lines),
                )

    def test_self_closing_raw_html_syntax_does_not_expose_a_title(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "mz"
            _package, _package_relative, html_relative = _write_game(game, "mz")
            line = "<textarea/><title>原题</title>"
            (game / html_relative).write_text(line + "\n", encoding="utf-8")

            locations = list(scan_game(game).locations)

            self.assertFalse(any(item["physical_file"] == html_relative for item in _derived(locations)))
            self.assertTrue(
                any(
                    item["physical_file"] == html_relative
                    and item["classification"] == "review"
                    and item["source_text"] == line
                    for item in locations
                )
            )

    def test_multiple_actual_html_titles_all_remain_review(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "mz"
            _package, _package_relative, html_relative = _write_game(game, "mz")
            (game / html_relative).write_text(
                "<head>\n<title>原题</title>\n<title>原题</title>\n</head>\n",
                encoding="utf-8",
            )

            locations = list(scan_game(game).locations)

            self.assertFalse(any(item["physical_file"] == html_relative for item in _derived(locations)))
            self.assertEqual(
                sum(
                    item["physical_file"] == html_relative
                    and item["classification"] == "review"
                    and item["source_text"] == "<title>原题</title>"
                    for item in locations
                ),
                2,
            )

    def test_form_feed_beside_the_title_keeps_the_line_in_review(self) -> None:
        with TemporaryDirectory() as temporary:
            game = Path(temporary) / "mz"
            _package, _package_relative, html_relative = _write_game(game, "mz")
            (game / html_relative).write_text("\f<title>原题</title>\n", encoding="utf-8")

            locations = list(scan_game(game).locations)

            self.assertEqual(
                sum(item["physical_file"] == html_relative for item in _derived(locations)),
                1,
            )
            self.assertTrue(
                any(
                    item["physical_file"] == html_relative
                    and item["classification"] == "review"
                    and item["source_text"] == "\f<title>原题</title>"
                    for item in locations
                )
            )


if __name__ == "__main__":
    unittest.main()
