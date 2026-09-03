from __future__ import annotations

import json
import sys
import tempfile
import tomllib
import unittest
from collections.abc import Iterator
from pathlib import Path
from typing import SupportsIndex
from unittest.mock import patch
from urllib.parse import quote as quote_url
from urllib.parse import unquote, urlsplit
from xml.etree import ElementTree

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

from att_toolbox.font_metadata import FontCoverage
from att_toolbox.font_references import (
    FontAsset,
    _AliasMatcher,
    _AliasTarget,
    _apply_text_patches,
    _discover_aliases,
    _fold_alias_text,
    _html_structure,
    _iter_alias_spans,
    _scan_css,
    _scan_generic_text,
    _scan_html,
    _scan_javascript,
    _scan_json,
    build_font_plan,
)


class FontReferenceEncodingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.game_root = Path("D:/fixtures/game")
        self.old_asset = FontAsset(
            path=self.game_root / "fonts" / "Old.ttf",
            relative_path="fonts/Old.ttf",
            size=1,
            sha256="old",
        )
        self.selected_name = "My Font & Kid's.ttf"
        self.selected_asset = FontAsset(
            path=self.game_root / "fonts" / self.selected_name,
            relative_path=f"fonts/{self.selected_name}",
            size=1,
            sha256="new",
        )

    def test_css_url_encodes_special_filename_and_round_trips(self) -> None:
        path = self.game_root / "styles" / "fonts.css"
        encoded_name = quote_url(self.selected_name, safe="")
        cases = (
            (
                "unquoted",
                "@font-face{src:url(../fonts/Old.ttf) format('truetype');}",
                f'url("../fonts/{encoded_name}")',
            ),
            (
                "single quoted",
                "@font-face{src:url('../fonts/Old.ttf') format('truetype');}",
                f"url('../fonts/{encoded_name}')",
            ),
        )
        for name, source, expected_url in cases:
            with self.subTest(name=name):
                patches, reviews = _scan_css(
                    path,
                    source,
                    game_root=self.game_root,
                    content_root=self.game_root,
                    assets=(self.old_asset,),
                    aliases={},
                    selected_name=self.selected_name,
                )
                updated = _apply_text_patches(source, patches)

                self.assertEqual(reviews, [])
                self.assertIn(expected_url, updated)
                self.assertEqual(
                    patches[0].references[0].new_value,
                    f"../fonts/{self.selected_name}",
                )
                reparsed, reparse_reviews = _scan_css(
                    path,
                    updated,
                    game_root=self.game_root,
                    content_root=self.game_root,
                    assets=(self.selected_asset,),
                    aliases={},
                    selected_name=self.selected_name,
                )
                self.assertEqual(reparse_reviews, [])
                self.assertEqual(len(reparsed), 1)

    def test_url_path_segment_encodes_fragment_and_percent_characters(self) -> None:
        path = self.game_root / "index.html"
        selected_name = "A#B%20 & Kid's.ttf"
        selected_asset = FontAsset(
            path=self.game_root / "fonts" / selected_name,
            relative_path=f"fonts/{selected_name}",
            size=1,
            sha256="reserved",
        )
        suffix = "?cache=1#face"
        source = (
            f'<link as="font" href="fonts/Old.ttf{suffix}"><div style="src:url(fonts/Old.ttf{suffix})"></div>'
        )
        patches, reviews = _scan_html(
            path,
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={},
            selected_name=selected_name,
        )
        updated = _apply_text_patches(source, patches)
        encoded_name = quote_url(selected_name, safe="")

        self.assertEqual(reviews, [])
        self.assertIn(f'href="fonts/{encoded_name}{suffix}"', updated)
        self.assertIn(f"url(fonts/{encoded_name}{suffix})", updated)
        self.assertEqual(
            [reference.new_value for patch in patches for reference in patch.references],
            [f"fonts/{selected_name}{suffix}", f"fonts/{selected_name}{suffix}"],
        )
        parsed_url = urlsplit(f"https://game.invalid/fonts/{encoded_name}{suffix}")
        self.assertEqual(unquote(Path(parsed_url.path).name), selected_name)
        self.assertEqual((parsed_url.query, parsed_url.fragment), ("cache=1", "face"))

        reparsed, reparse_reviews = _scan_html(
            path,
            updated,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(selected_asset,),
            aliases={},
            selected_name=selected_name,
        )
        self.assertEqual(reparse_reviews, [])
        self.assertEqual(len(reparsed), 2)

    def test_html_attributes_encode_both_html_and_css_layers(self) -> None:
        path = self.game_root / "index.html"
        source = "<link as='font' href='fonts/Old.ttf'><div style=\"src:url(fonts/Old.ttf)\"></div>"
        patches, reviews = _scan_html(
            path,
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={},
            selected_name=self.selected_name,
        )
        updated = _apply_text_patches(source, patches)
        encoded_name = quote_url(self.selected_name, safe="")

        self.assertEqual(reviews, [])
        self.assertIn(f"href='fonts/{encoded_name}'", updated)
        self.assertIn(f"url(fonts/{encoded_name})", updated)

        reparsed, reparse_reviews = _scan_html(
            path,
            updated,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.selected_asset,),
            aliases={},
            selected_name=self.selected_name,
        )
        self.assertEqual(reparse_reviews, [])
        self.assertEqual(
            len(reparsed),
            2,
        )

    def test_css_font_family_uses_css_and_html_attribute_encoding(self) -> None:
        old_aliases = {"old": _AliasTarget(self.old_asset, False)}
        selected_aliases = {
            Path(self.selected_name).stem.casefold(): _AliasTarget(self.selected_asset, False)
        }
        cases = (
            ("standalone", self.game_root / "style.css", "body{font-family:Old;}", _scan_css),
            (
                "single html quote",
                self.game_root / "index.html",
                "<div style='font-family:Old;'></div>",
                _scan_html,
            ),
            (
                "double html quote",
                self.game_root / "index.html",
                '<div style="font-family:Old;"></div>',
                _scan_html,
            ),
        )
        for name, path, source, scanner in cases:
            with self.subTest(name=name):
                patches, reviews = scanner(
                    path,
                    source,
                    game_root=self.game_root,
                    content_root=self.game_root,
                    assets=(self.old_asset,),
                    aliases=old_aliases,
                    selected_name=self.selected_name,
                )
                updated = _apply_text_patches(source, patches)

                self.assertEqual(reviews, [])
                self.assertNotIn("font-family:My Font", updated)
                self.assertEqual(
                    patches[0].references[0].new_value,
                    Path(self.selected_name).stem,
                )
                reparsed, reparse_reviews = scanner(
                    path,
                    updated,
                    game_root=self.game_root,
                    content_root=self.game_root,
                    assets=(self.selected_asset,),
                    aliases=selected_aliases,
                    selected_name=self.selected_name,
                )
                self.assertEqual(reparse_reviews, [])
                self.assertEqual(len(reparsed), 1)

    def test_registered_css_family_alias_keeps_its_original_spelling(self) -> None:
        source = "body{font-family:Old;}"
        patches, reviews = _scan_css(
            self.game_root / "style.css",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old": _AliasTarget(self.old_asset, True)},
            selected_name=self.selected_name,
        )

        self.assertEqual(reviews, [])
        self.assertEqual(_apply_text_patches(source, patches), source)

    def test_css_comments_do_not_create_font_references(self) -> None:
        aliases = {"old": _AliasTarget(self.old_asset, False)}
        css = "/* @font-face{src:url(fonts/Old.ttf)} body{font-family:Old;} */"

        css_patches, css_reviews = _scan_css(
            self.game_root / "style.css",
            css,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases=aliases,
            selected_name=self.selected_name,
        )
        html = f'<style>{css}</style><div style="{css}"></div>'
        html_patches, html_reviews = _scan_html(
            self.game_root / "index.html",
            html,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases=aliases,
            selected_name=self.selected_name,
        )

        self.assertEqual((css_patches, css_reviews), ([], []))
        self.assertEqual((html_patches, html_reviews), ([], []))

    def test_css_url_text_inside_an_ordinary_string_is_not_a_reference(self) -> None:
        source = 'body::before{content:"url(fonts/Old.ttf)";}'

        patches, reviews = _scan_css(
            self.game_root / "style.css",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )

        self.assertEqual((patches, reviews), ([], []))

    def test_css_font_face_decodes_family_escapes_when_building_aliases(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary)
            stylesheet = game_root / "style.css"
            stylesheet.write_text(
                r"@font-face{font-family:O\6c d;src:url(fonts/Old.ttf);}",
                encoding="utf-8",
            )
            asset = FontAsset(
                path=game_root / "fonts" / "Old.ttf",
                relative_path="fonts/Old.ttf",
                size=1,
                sha256="old",
            )

            aliases, mapping, reviews = _discover_aliases(
                game_root=game_root,
                content_root=game_root,
                files=(stylesheet,),
                assets=(asset,),
                runtime_javascript=frozenset(),
            )

        self.assertEqual(reviews, [])
        self.assertIn("old", mapping)
        self.assertTrue(mapping["old"].preserve_value)
        self.assertIn("Old", [alias.value for alias in aliases])

    def test_css_font_face_uses_unique_local_fallback_and_reports_remote_url(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary)
            stylesheet = game_root / "style.css"
            stylesheet.write_text(
                "@font-face{font-family:RuntimeFont;"
                "src:url(https://cdn.example/x.woff2),url(fonts/Old.ttf);}",
                encoding="utf-8",
            )
            asset = FontAsset(
                path=game_root / "fonts" / "Old.ttf",
                relative_path="fonts/Old.ttf",
                size=1,
                sha256="old",
            )

            aliases, mapping, reviews = _discover_aliases(
                game_root=game_root,
                content_root=game_root,
                files=(stylesheet,),
                assets=(asset,),
                runtime_javascript=frozenset(),
            )

        self.assertEqual([review.reason for review in reviews], ["unresolved_font_face_asset"])
        self.assertEqual(reviews[0].value, "https://cdn.example/x.woff2")
        self.assertEqual(
            [(alias.value, alias.asset, alias.basis) for alias in aliases if alias.value == "RuntimeFont"],
            [("RuntimeFont", "fonts/Old.ttf", "css_font_face")],
        )
        self.assertEqual(mapping["runtimefont"].asset, asset)
        self.assertTrue(mapping["runtimefont"].preserve_value)

    def test_css_font_face_with_multiple_local_assets_does_not_fall_back_to_stem(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary)
            stylesheet = game_root / "style.css"
            stylesheet.write_text(
                "@font-face{font-family:First;src:url(fonts/First.ttf),url(fonts/Second.ttf);}",
                encoding="utf-8",
            )
            first = FontAsset(
                path=game_root / "fonts" / "First.ttf",
                relative_path="fonts/First.ttf",
                size=1,
                sha256="first",
            )
            second = FontAsset(
                path=game_root / "fonts" / "Second.ttf",
                relative_path="fonts/Second.ttf",
                size=1,
                sha256="second",
            )

            aliases, mapping, reviews = _discover_aliases(
                game_root=game_root,
                content_root=game_root,
                files=(stylesheet,),
                assets=(first, second),
                runtime_javascript=frozenset(),
            )

        self.assertEqual(
            [review.reason for review in reviews],
            ["css_font_face_maps_to_multiple_assets"],
        )
        self.assertNotIn("first", mapping)
        self.assertNotIn("First", [alias.value for alias in aliases])
        self.assertIn("second", mapping)

    def test_build_font_plan_keeps_registered_family_with_remote_first_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            game_root = root / "game"
            fonts = game_root / "fonts"
            plugins = game_root / "js" / "plugins.js"
            fonts.mkdir(parents=True)
            plugins.parent.mkdir(parents=True)
            old_font = fonts / "Old.ttf"
            old_font.write_bytes(b"old-font")
            selected_font = root / "Replacement.ttf"
            selected_font.write_bytes(b"replacement-font")
            plugins.write_text("var $plugins = [];\n", encoding="utf-8")
            stylesheet = game_root / "style.css"
            stylesheet.write_text(
                "@font-face{font-family:Old;"
                "src:url(https://cdn.example/x.woff2),url(fonts/Old.ttf);}\n"
                "body{font-family:Old;}",
                encoding="utf-8",
            )

            with patch(
                "att_toolbox.font_references.check_font_coverage",
                return_value=FontCoverage("", "", 1),
            ):
                plan = build_font_plan(
                    game_root=game_root,
                    content_root=game_root,
                    selected_font=selected_font,
                )

        self.assertIn(
            "unresolved_font_face_asset",
            [review.reason for review in plan.reviews],
        )
        family_aliases = [alias for alias in plan.aliases if alias.value == "Old"]
        self.assertEqual([alias.basis for alias in family_aliases], ["css_font_face"])
        stylesheet_mutation = next(
            mutation for mutation in plan.mutations if mutation.relative_path == "style.css"
        )
        updated = stylesheet_mutation.replacement.decode("utf-8")
        self.assertEqual(updated.count("font-family:Old"), 2)
        self.assertIn('url("fonts/Replacement.ttf")', updated)
        self.assertIn("url(https://cdn.example/x.woff2)", updated)

    def test_build_font_plan_uses_last_valid_font_face_descriptors(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            game_root = root / "game"
            fonts = game_root / "fonts"
            plugins = game_root / "js" / "plugins.js"
            fonts.mkdir(parents=True)
            plugins.parent.mkdir(parents=True)
            (fonts / "Old.ttf").write_bytes(b"old-font")
            (fonts / "Obsolete.ttf").write_bytes(b"obsolete-font")
            selected_font = root / "Replacement.ttf"
            selected_font.write_bytes(b"replacement-font")
            plugins.write_text("var $plugins = [];\n", encoding="utf-8")
            stylesheet = game_root / "style.css"
            stylesheet.write_text(
                "@font-face{"
                "font-family:Ignored;font-family:RuntimeFont;font-family:Bad,;"
                "src:url(fonts/Obsolete.ttf);"
                "src:url(data:font/woff2;base64,AAAA;BBBB),url(fonts/Old.ttf);"
                "src:url(;"
                "}\nbody{font-family:RuntimeFont;}",
                encoding="utf-8",
            )

            with patch(
                "att_toolbox.font_references.check_font_coverage",
                return_value=FontCoverage("", "", 1),
            ):
                plan = build_font_plan(
                    game_root=game_root,
                    content_root=game_root,
                    selected_font=selected_font,
                )

        runtime_alias = next(alias for alias in plan.aliases if alias.value == "RuntimeFont")
        self.assertEqual(runtime_alias.asset, "fonts/Old.ttf")
        stylesheet_mutation = next(
            mutation for mutation in plan.mutations if mutation.relative_path == "style.css"
        )
        updated = stylesheet_mutation.replacement.decode("utf-8")
        self.assertIn("url(fonts/Obsolete.ttf)", updated)
        self.assertIn("url(data:font/woff2;base64,AAAA;BBBB)", updated)
        self.assertIn('url("fonts/Replacement.ttf")', updated)
        self.assertIn("font-family:RuntimeFont", updated)

    def test_build_font_plan_ignores_invalid_late_source_descriptors(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            game_root = root / "game"
            fonts = game_root / "fonts"
            plugins = game_root / "js" / "plugins.js"
            fonts.mkdir(parents=True)
            plugins.parent.mkdir(parents=True)
            (fonts / "Old.ttf").write_bytes(b"old-font")
            selected_font = root / "Replacement.ttf"
            selected_font.write_bytes(b"replacement-font")
            plugins.write_text("var $plugins = [];\n", encoding="utf-8")
            stylesheet = game_root / "style.css"
            stylesheet.write_text(
                "@font-face{font-family:RuntimeFont;"
                'src:local(Installed),bogus(),url(fonts/Old.ttf);src:"local(fake)";}\n'
                "body{font-family:RuntimeFont;}",
                encoding="utf-8",
            )

            matcher_builder = _AliasMatcher.for_aliases
            with (
                patch(
                    "att_toolbox.font_references.check_font_coverage",
                    return_value=FontCoverage("", "", 1),
                ),
                patch.object(
                    _AliasMatcher,
                    "for_aliases",
                    wraps=matcher_builder,
                ) as build_matcher,
            ):
                plan = build_font_plan(
                    game_root=game_root,
                    content_root=game_root,
                    selected_font=selected_font,
                )

        self.assertEqual(build_matcher.call_count, 1)
        runtime_alias = next(alias for alias in plan.aliases if alias.value == "RuntimeFont")
        self.assertEqual(runtime_alias.asset, "fonts/Old.ttf")
        stylesheet_mutation = next(
            mutation for mutation in plan.mutations if mutation.relative_path == "style.css"
        )
        updated = stylesheet_mutation.replacement.decode("utf-8")
        self.assertIn('url("fonts/Replacement.ttf")', updated)
        self.assertEqual(updated.count("font-family:RuntimeFont"), 2)

    def test_build_font_plan_skips_url_with_unsupported_tech_hint(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            game_root = root / "game"
            fonts = game_root / "fonts"
            plugins = game_root / "js" / "plugins.js"
            fonts.mkdir(parents=True)
            plugins.parent.mkdir(parents=True)
            (fonts / "Old.ttf").write_bytes(b"old-font")
            (fonts / "New.ttf").write_bytes(b"new-font")
            selected_font = root / "Replacement.ttf"
            selected_font.write_bytes(b"replacement-font")
            plugins.write_text("var $plugins = [];\n", encoding="utf-8")
            stylesheet = game_root / "style.css"
            stylesheet.write_text(
                "@font-face{font-family:RuntimeFont;"
                "src:url(fonts/Old.ttf);src:url(fonts/New.ttf) tech(bogus);}\n"
                "body{font-family:RuntimeFont;}",
                encoding="utf-8",
            )

            with patch(
                "att_toolbox.font_references.check_font_coverage",
                return_value=FontCoverage("", "", 1),
            ):
                plan = build_font_plan(
                    game_root=game_root,
                    content_root=game_root,
                    selected_font=selected_font,
                )

        runtime_alias = next(alias for alias in plan.aliases if alias.value == "RuntimeFont")
        self.assertEqual(runtime_alias.asset, "fonts/Old.ttf")
        stylesheet_mutation = next(
            mutation for mutation in plan.mutations if mutation.relative_path == "style.css"
        )
        updated = stylesheet_mutation.replacement.decode("utf-8")
        self.assertIn('url("fonts/Replacement.ttf")', updated)
        self.assertIn("url(fonts/New.ttf) tech(bogus)", updated)

    def test_alias_candidates_use_one_joint_scan_and_preserve_overlaps(self) -> None:
        aliases = {
            **{f"unused-{index}": _AliasTarget(self.old_asset, False) for index in range(128)},
            "old": _AliasTarget(self.old_asset, False),
            "old font": _AliasTarget(self.old_asset, False),
        }
        text = "xOld OLD old-font fonts/Old.ttf Old Font"

        with patch(
            "att_toolbox.font_references._fold_alias_text",
            wraps=_fold_alias_text,
        ) as fold_text:
            spans = list(_iter_alias_spans(text, _AliasMatcher.for_aliases(aliases)))

        values = [text[start:end] for start, end in spans]
        self.assertEqual(fold_text.call_count, 1)
        self.assertEqual(values.count("OLD"), 1)
        self.assertEqual(values.count("Old"), 2)
        self.assertIn("Old Font", values)
        self.assertNotIn("xOld", values)
        self.assertNotIn("old", values)

    def test_json_and_javascript_batches_build_one_alias_matcher(self) -> None:
        aliases = {"old": _AliasTarget(self.old_asset, False)}
        cases = (
            (
                _scan_json,
                self.game_root / "values.json",
                json.dumps([f"prefix Old suffix {index}" for index in range(16)]),
                "unresolved_json_font_value",
            ),
            (
                _scan_javascript,
                self.game_root / "values.js",
                "\n".join(f'const value{index} = "prefix Old suffix {index}";' for index in range(16)),
                "unresolved_javascript_font_value",
            ),
        )
        for scanner, path, source, expected_reason in cases:
            with self.subTest(path=path.name):
                matcher_builder = _AliasMatcher.for_aliases
                with patch.object(
                    _AliasMatcher,
                    "for_aliases",
                    wraps=matcher_builder,
                ) as build_matcher:
                    patches, reviews = scanner(
                        path,
                        source,
                        game_root=self.game_root,
                        content_root=self.game_root,
                        assets=(self.old_asset,),
                        aliases=aliases,
                        selected_name=self.selected_name,
                    )

                self.assertEqual(build_matcher.call_count, 1)
                self.assertEqual(patches, [])
                self.assertEqual(len(reviews), 16)
                self.assertEqual({review.reason for review in reviews}, {expected_reason})

    def test_build_font_plan_does_not_report_unicode_alias_prefixes_in_inactive_script(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            game_root = root / "game"
            fonts = game_root / "fonts"
            scripts = game_root / "js"
            fonts.mkdir(parents=True)
            scripts.mkdir(parents=True)
            (fonts / "İ.ttf").write_bytes(b"capital-i-dot")
            (fonts / "a.ttf").write_bytes(b"a-font")
            selected_font = root / "Replacement.ttf"
            selected_font.write_bytes(b"replacement-font")
            (scripts / "plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
            (scripts / "inactive.js").write_text(
                'const first = "İx"; const second = "a\u0301x";\n',
                encoding="utf-8",
            )

            with patch(
                "att_toolbox.font_references.check_font_coverage",
                return_value=FontCoverage("", "", 1),
            ):
                plan = build_font_plan(
                    game_root=game_root,
                    content_root=game_root,
                    selected_font=selected_font,
                )

        self.assertNotIn(
            "inactive_or_unproven_javascript_font_consumer",
            [review.reason for review in plan.reviews],
        )

    def test_html_comment_lookup_count_depends_on_comments_not_tags(self) -> None:
        class CountingText(str):
            comment_lookups = 0

            def find(
                self,
                sub: str,
                start: SupportsIndex | None = 0,
                end: SupportsIndex | None = None,
                /,
            ) -> int:
                if sub == "<!--":
                    self.comment_lookups += 1
                if end is None:
                    return super().find(sub, start)
                return super().find(sub, start, end)

        text = CountingText(
            "".join(f"<div data-index='{index}'></div>" for index in range(128))
            + "<!-- <style>ignored</style> -->"
            + "".join(f"<span>{index}</span>" for index in range(128))
            + "<style>body{font-family:Old}</style>"
        )

        tags, regions = _html_structure(text)

        self.assertEqual(text.comment_lookups, 2)
        self.assertEqual(len(tags), 257)
        self.assertEqual(
            [(region.kind, text[region.start : region.end]) for region in regions],
            [
                ("style", "body{font-family:Old}"),
            ],
        )

    def test_css_family_comments_are_removed_without_shifting_source_spans(self) -> None:
        source = "body{font-family:Old/**/;}"
        patches, reviews = _scan_css(
            self.game_root / "style.css",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 1)
        self.assertEqual(patches[0].original, "Old")
        self.assertIn("/**/", _apply_text_patches(source, patches))

        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary)
            stylesheet = game_root / "style.css"
            stylesheet.write_text(
                "@font-face{font-family:Old/**/;src:url(fonts/Old.ttf);}\n"
                '@font-face{font-family:"Old}Name";src:url(fonts/Old.ttf);}',
                encoding="utf-8",
            )
            asset = FontAsset(
                path=game_root / "fonts" / "Old.ttf",
                relative_path="fonts/Old.ttf",
                size=1,
                sha256="old",
            )
            _aliases, mapping, alias_reviews = _discover_aliases(
                game_root=game_root,
                content_root=game_root,
                files=(stylesheet,),
                assets=(asset,),
                runtime_javascript=frozenset(),
            )

        self.assertEqual(alias_reviews, [])
        self.assertIn("old", mapping)
        self.assertIn("old}name", mapping)

    def test_css_comment_between_unquoted_family_tokens_keeps_the_separator(self) -> None:
        source = "body{font-family:Old/**/Font;}"
        patches, reviews = _scan_css(
            self.game_root / "style.css",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old font": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 1)
        self.assertEqual(patches[0].references[0].old_value, "Old Font")

    def test_css_escaped_bang_is_part_of_an_unquoted_family(self) -> None:
        source = r"body{font-family:Old\! important;}"
        patches, reviews = _scan_css(
            self.game_root / "style.css",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old! important": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 1)
        self.assertEqual(patches[0].references[0].old_value, "Old! important")

    def test_css_family_parser_respects_escapes_quotes_and_declaration_boundaries(self) -> None:
        source = (
            r"a{font-family:Old\,Name;}"
            r"b{font-family:Old\;Name;}"
            r"c{font-family:Old\}Name;}"
            'd{font-family:"Old,;}Name";}'
            "e{--choice:font-family:Old;}"
        )
        aliases = {
            name.casefold(): _AliasTarget(self.old_asset, False)
            for name in ("Old,Name", "Old;Name", "Old}Name", "Old,;}Name")
        }
        patches, reviews = _scan_css(
            self.game_root / "style.css",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases=aliases,
            selected_name=self.selected_name,
        )
        updated = _apply_text_patches(source, patches)

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 4)
        self.assertIn("--choice:font-family:Old", updated)

        malformed = "body{font-family:Old\\"
        malformed_patches, malformed_reviews = _scan_css(
            self.game_root / "style.css",
            malformed,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases=aliases,
            selected_name=self.selected_name,
        )
        self.assertEqual(malformed_patches, [])
        self.assertEqual(
            [review.reason for review in malformed_reviews],
            ["unparsed_css_font_family_list"],
        )

    def test_javascript_only_encodes_the_mv_loader_url_argument(self) -> None:
        path = self.game_root / "js" / "plugins" / "fonts.js"
        source = (
            'Graphics.loadFont("GameFont", "../../fonts/Old.ttf?cache=1#face");\n'
            'FontManager.load("rmmz-mainfont", "Old.ttf");\n'
            'const settings = { fontFile: "Old.ttf" };\n'
        )
        patches, reviews = _scan_javascript(
            path,
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={},
            selected_name=self.selected_name,
        )
        updated = _apply_text_patches(source, patches)
        encoded_name = quote_url(self.selected_name, safe="")

        self.assertEqual(reviews, [])
        self.assertIn(f'"../../fonts/{encoded_name}?cache=1#face"', updated)
        self.assertEqual(updated.count(f'"{self.selected_name}"'), 2)
        self.assertNotIn(f'FontManager.load("rmmz-mainfont", "{encoded_name}")', updated)

    def test_fontface_source_rewrites_nested_css_url_and_format_without_changing_alias(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary)
            script = game_root / "js" / "plugins" / "fonts.js"
            script.parent.mkdir(parents=True)
            source = (
                'const face = new FontFace("GameFont", '
                "\"url('../../fonts/Old.ttf?cache=1#face') format('woff2')\");\n"
                'const alias = "RuntimeFont";\n'
                "const variableFace = new FontFace(alias, \"url('../../fonts/Old.ttf')\");"
            )
            script.write_text(source, encoding="utf-8")
            old_asset = FontAsset(
                path=game_root / "fonts" / "Old.ttf",
                relative_path="fonts/Old.ttf",
                size=1,
                sha256="old",
            )
            selected_name = "A# B.ttf"

            _aliases, mapping, alias_reviews = _discover_aliases(
                game_root=game_root,
                content_root=game_root,
                files=(script,),
                assets=(old_asset,),
                runtime_javascript=frozenset({script.resolve(strict=True)}),
            )
            patches, reviews = _scan_javascript(
                script,
                source,
                game_root=game_root,
                content_root=game_root,
                assets=(old_asset,),
                aliases=mapping,
                selected_name=selected_name,
            )
            updated = _apply_text_patches(source, patches)

        self.assertEqual(alias_reviews, [])
        self.assertIn("gamefont", mapping)
        self.assertTrue(mapping["gamefont"].preserve_value)
        self.assertEqual(reviews, [])
        self.assertIn('new FontFace("GameFont",', updated)
        self.assertIn("new FontFace(alias,", updated)
        self.assertIn("../../fonts/A%23%20B.ttf?cache=1#face", updated)
        self.assertEqual(updated.count("../../fonts/A%23%20B.ttf"), 2)
        self.assertIn("format('truetype')", updated)

    def test_javascript_fontface_alias_uses_all_fallback_urls(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary)
            script = game_root / "js" / "plugins" / "fonts.js"
            script.parent.mkdir(parents=True)
            source = (
                'new FontFace("RuntimeFont", '
                "\"url('https://cdn.example/Remote.woff2'),url('../../fonts/Old.ttf')\");"
            )
            script.write_text(source, encoding="utf-8")
            old_asset = FontAsset(
                path=game_root / "fonts" / "Old.ttf",
                relative_path="fonts/Old.ttf",
                size=1,
                sha256="old",
            )

            aliases, mapping, reviews = _discover_aliases(
                game_root=game_root,
                content_root=game_root,
                files=(script,),
                assets=(old_asset,),
                runtime_javascript=frozenset({script.resolve(strict=True)}),
            )

        self.assertEqual([review.value for review in reviews], ["https://cdn.example/Remote.woff2"])
        self.assertEqual(mapping["runtimefont"].asset, old_asset)
        self.assertTrue(mapping["runtimefont"].preserve_value)
        self.assertEqual(
            [(alias.value, alias.asset) for alias in aliases if alias.value == "RuntimeFont"],
            [("RuntimeFont", "fonts/Old.ttf")],
        )

    def test_javascript_fontface_multiple_local_fallbacks_are_ambiguous(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary)
            script = game_root / "fonts.js"
            script.write_text(
                "new FontFace(\"First\", \"url('fonts/First.ttf'),url('fonts/Second.ttf')\");",
                encoding="utf-8",
            )
            assets = tuple(
                FontAsset(
                    path=game_root / "fonts" / name,
                    relative_path=f"fonts/{name}",
                    size=1,
                    sha256=name,
                )
                for name in ("First.ttf", "Second.ttf")
            )

            aliases, mapping, reviews = _discover_aliases(
                game_root=game_root,
                content_root=game_root,
                files=(script,),
                assets=assets,
                runtime_javascript=frozenset({script.resolve(strict=True)}),
            )

        self.assertIn("javascript_fontface_maps_to_multiple_assets", [item.reason for item in reviews])
        self.assertNotIn("first", mapping)
        self.assertNotIn("First", [alias.value for alias in aliases])

    def test_css_scanner_indexes_assets_once_for_many_references(self) -> None:
        class CountingAssets(list[FontAsset]):
            def __init__(self, values: tuple[FontAsset, ...]) -> None:
                super().__init__(values)
                self.iterations = 0

            def __iter__(self) -> Iterator[FontAsset]:
                self.iterations += 1
                return super().__iter__()

        assets = CountingAssets(
            (
                self.old_asset,
                *(
                    FontAsset(
                        path=self.game_root / "fonts" / f"Unused{index}.ttf",
                        relative_path=f"fonts/Unused{index}.ttf",
                        size=1,
                        sha256=str(index),
                    )
                    for index in range(200)
                ),
            )
        )
        source = "\n".join(f".f{index}{{src:url(fonts/Old.ttf)}}" for index in range(200))

        patches, reviews = _scan_css(
            self.game_root / "style.css",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=assets,
            aliases={},
            selected_name=self.selected_name,
        )

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 200)
        self.assertEqual(assets.iterations, 1)
        self.assertEqual(
            _apply_text_patches(source, patches).count(quote_url(self.selected_name, safe="")),
            200,
        )

    def test_css_src_boundaries_keep_quoted_and_escaped_delimiters_inside_declaration(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary)
            stylesheet = game_root / "style.css"
            source = (
                '@font-face{font-family:"Old;}Alias";'
                'src:local("Old;} Local"),'
                r"local(Old\;Local),"
                'url("fonts/Old.ttf") format("woff2");}'
            )
            stylesheet.write_text(source, encoding="utf-8")
            asset = FontAsset(
                path=game_root / "fonts" / "Old.ttf",
                relative_path="fonts/Old.ttf",
                size=1,
                sha256="old",
            )

            _aliases, mapping, alias_reviews = _discover_aliases(
                game_root=game_root,
                content_root=game_root,
                files=(stylesheet,),
                assets=(asset,),
                runtime_javascript=frozenset(),
            )
            patches, reviews = _scan_css(
                stylesheet,
                source,
                game_root=game_root,
                content_root=game_root,
                assets=(asset,),
                aliases=mapping,
                selected_name=self.selected_name,
            )
            updated = _apply_text_patches(source, patches)

        self.assertEqual(alias_reviews, [])
        self.assertIn("old;}alias", mapping)
        self.assertEqual(reviews, [])
        self.assertIn(quote_url(self.selected_name, safe=""), updated)
        self.assertIn('format("truetype")', updated)

    def test_css_font_family_preserves_important_priority(self) -> None:
        source = 'a{font-family:Old !important}b{font-family:"Old"/**/! important;}'
        patches, reviews = _scan_css(
            self.game_root / "style.css",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )
        updated = _apply_text_patches(source, patches)

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 2)
        self.assertIn(" !important", updated)
        self.assertIn("/**/! important", updated)
        self.assertNotIn("font-family:Old", updated)

    def test_css_quoted_url_accepts_line_continuation_and_bare_url_requests_review(self) -> None:
        quoted = '@font-face{src:url("fonts/Ol\\\nd.ttf");}'
        quoted_patches, quoted_reviews = _scan_css(
            self.game_root / "style.css",
            quoted,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={},
            selected_name=self.selected_name,
        )

        self.assertEqual(quoted_reviews, [])
        self.assertEqual(len(quoted_patches), 1)
        self.assertIn(quote_url(self.selected_name, safe=""), _apply_text_patches(quoted, quoted_patches))

        bare = "@font-face{src:url(fonts/Ol\\\nd.ttf);}"
        bare_patches, bare_reviews = _scan_css(
            self.game_root / "style.css",
            bare,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={},
            selected_name=self.selected_name,
        )

        self.assertEqual(bare_patches, [])
        self.assertEqual([review.reason for review in bare_reviews], ["unparsed_css_font_url"])

    def test_inline_style_decodes_html_entities_before_css_and_maps_patches_back(self) -> None:
        source = '<div style="font-family:O&#108;d;src:url(&quot;fonts/O&#x6c;d.ttf&quot;);"></div>'
        aliases = {"old": _AliasTarget(self.old_asset, False)}
        patches, reviews = _scan_html(
            self.game_root / "index.html",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases=aliases,
            selected_name=self.selected_name,
        )
        updated = _apply_text_patches(source, patches)

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 2)
        self.assertNotIn("O&#108;d", updated)
        self.assertNotIn("O&#x6c;d.ttf", updated)
        self.assertIn("&quot;fonts/", updated)

        selected_aliases = {
            Path(self.selected_name).stem.casefold(): _AliasTarget(self.selected_asset, False)
        }
        reparsed, reparse_reviews = _scan_html(
            self.game_root / "index.html",
            updated,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.selected_asset,),
            aliases=selected_aliases,
            selected_name=self.selected_name,
        )
        self.assertEqual(reparse_reviews, [])
        self.assertEqual(len(reparsed), 2)

    def test_json_nonstandard_constants_are_rejected_before_font_rewrite(self) -> None:
        aliases = {"old": _AliasTarget(self.old_asset, False)}
        for constant in ("NaN", "Infinity", "-Infinity"):
            with self.subTest(constant=constant):
                source = f'{{"fontName":"Old","metric":{constant}}}'
                patches, reviews = _scan_json(
                    self.game_root / "settings.json",
                    source,
                    game_root=self.game_root,
                    content_root=self.game_root,
                    assets=(self.old_asset,),
                    aliases=aliases,
                    selected_name=self.selected_name,
                )

                self.assertEqual(patches, [])
                self.assertEqual(
                    [review.reason for review in reviews],
                    ["invalid_json_with_possible_font_reference"],
                )

    def test_xml_and_toml_encode_values_with_their_own_syntax(self) -> None:
        old_aliases = {"old": _AliasTarget(self.old_asset, False)}
        selected_aliases = {
            Path(self.selected_name).stem.casefold(): _AliasTarget(self.selected_asset, False)
        }
        stem = Path(self.selected_name).stem

        xml_path = self.game_root / "fonts.xml"
        xml_source = "<root><fontName>Old</fontName><item fontName='Old'/></root>"
        xml_patches, xml_reviews = _scan_generic_text(
            xml_path,
            xml_source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases=old_aliases,
            selected_name=self.selected_name,
        )
        updated_xml = _apply_text_patches(xml_source, xml_patches)
        xml_root = ElementTree.fromstring(updated_xml)

        self.assertEqual(xml_reviews, [])
        self.assertEqual(xml_root.findtext("fontName"), stem)
        item = xml_root.find("item")
        self.assertIsNotNone(item)
        assert item is not None
        self.assertEqual(item.get("fontName"), stem)
        reparsed_xml, xml_reparse_reviews = _scan_generic_text(
            xml_path,
            updated_xml,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.selected_asset,),
            aliases=selected_aliases,
            selected_name=self.selected_name,
        )
        self.assertEqual(xml_reparse_reviews, [])
        self.assertEqual(len(reparsed_xml), 2)

        toml_path = self.game_root / "fonts.toml"
        toml_source = "fontName = 'Old'\n"
        toml_patches, toml_reviews = _scan_generic_text(
            toml_path,
            toml_source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases=old_aliases,
            selected_name=self.selected_name,
        )
        updated_toml = _apply_text_patches(toml_source, toml_patches)

        self.assertEqual(toml_reviews, [])
        self.assertEqual(tomllib.loads(updated_toml)["fontName"], stem)
        reparsed_toml, toml_reparse_reviews = _scan_generic_text(
            toml_path,
            updated_toml,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.selected_asset,),
            aliases=selected_aliases,
            selected_name=self.selected_name,
        )
        self.assertEqual(toml_reparse_reviews, [])
        self.assertEqual(len(reparsed_toml), 1)

    def test_xml_numeric_character_references_are_decoded_as_complete_values(self) -> None:
        source = '<root><fontName>Old&#32;Font</fontName><item fontName="Old&#x20;Font"/></root>'
        patches, reviews = _scan_generic_text(
            self.game_root / "fonts.xml",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old font": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )
        updated = _apply_text_patches(source, patches)
        root = ElementTree.fromstring(updated)

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 2)
        self.assertEqual(root.findtext("fontName"), Path(self.selected_name).stem)
        item = root.find("item")
        self.assertIsNotNone(item)
        assert item is not None
        self.assertEqual(item.get("fontName"), Path(self.selected_name).stem)

    def test_alias_review_uses_token_boundaries_and_ignores_config_comments(self) -> None:
        aliases = {"old": _AliasTarget(self.old_asset, False)}
        cases = (
            (self.game_root / "settings.json", '{"fontWeight":"bold"}'),
            (self.game_root / "fonts.xml", "<root><!-- Old --><weight>bold</weight></root>"),
            (self.game_root / "fonts.toml", "# fontName = 'Old'\nfontWeight = 'bold'\n"),
            (self.game_root / "fonts.ini", "; fontName=Old\nfontWeight=bold\n"),
        )
        for path, source in cases:
            with self.subTest(path=path.name):
                patches, reviews = _scan_generic_text(
                    path,
                    source,
                    game_root=self.game_root,
                    content_root=self.game_root,
                    assets=(self.old_asset,),
                    aliases=aliases,
                    selected_name=self.selected_name,
                )

                self.assertEqual((patches, reviews), ([], []))

    def test_ini_font_candidate_requires_review(self) -> None:
        patches, reviews = _scan_generic_text(
            self.game_root / "fonts.ini",
            "fontName=Old\n",
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )

        self.assertEqual(patches, [])
        self.assertEqual([review.reason for review in reviews], ["ini_font_value_requires_review"])

    def test_xml_cdata_is_not_rewritten_as_a_field(self) -> None:
        aliases = {"old": _AliasTarget(self.old_asset, False)}
        patches, reviews = _scan_generic_text(
            self.game_root / "fonts.xml",
            "<root><![CDATA[<fontName>Old</fontName>]]></root>",
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases=aliases,
            selected_name=self.selected_name,
        )

        self.assertEqual(patches, [])
        self.assertEqual(
            [review.reason for review in reviews],
            ["unclassified_or_partial_xml_font_context"],
        )

    def test_toml_multiline_strings_and_comments_do_not_block_single_line_font_value(self) -> None:
        source = (
            "note = '''\nfontName = 'Old'\n# still text\n'''\n"
            'other = """\nOld.ttf\n"""\n'
            "# fontName = 'Old'\n"
            "fontName = 'Old'\n"
        )
        patches, reviews = _scan_generic_text(
            self.game_root / "fonts.toml",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )
        updated = _apply_text_patches(source, patches)
        parsed = tomllib.loads(updated)

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 1)
        self.assertEqual(parsed["note"], "fontName = 'Old'\n# still text\n")
        self.assertEqual(parsed["other"], "Old.ttf\n")
        self.assertEqual(parsed["fontName"], Path(self.selected_name).stem)

    def test_toml_dotted_bare_key_uses_every_semantic_path_segment(self) -> None:
        source = "font.name = 'Old'\n"
        patches, reviews = _scan_generic_text(
            self.game_root / "fonts.toml",
            source,
            game_root=self.game_root,
            content_root=self.game_root,
            assets=(self.old_asset,),
            aliases={"old": _AliasTarget(self.old_asset, False)},
            selected_name=self.selected_name,
        )
        updated = _apply_text_patches(source, patches)

        self.assertEqual(reviews, [])
        self.assertEqual(len(patches), 1)
        self.assertEqual(tomllib.loads(updated)["font"]["name"], Path(self.selected_name).stem)


if __name__ == "__main__":
    unittest.main()
