from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

from att_skill_tools import ToolError
from att_toolbox.font_references import FontAsset, _apply_text_patches, _scan_javascript
from att_toolbox.js import scan_javascript
from att_toolbox.roundtrip import replace_reviewed_javascript_literal


class JavaScriptRewriteTests(unittest.TestCase):
    def test_statement_regex_contents_are_not_rewriteable_literals(self) -> None:
        prefixes = (
            "if (ready)",
            "if /* condition */ ((ready))",
            "while (ready)",
            "for (let i = 0; i < 1; i++)",
            "for (const item of items)",
            "with (context)",
            "if (ready) {} else",
            "if (ready) {}",
            "try {} catch (error)",
            "function check() {}",
            "class Check {}",
            "class Check extends (Base) {}",
            "const options = {function: 1}; if (ready)",
            "const options = {class: 1}; if (ready) {}",
            "const options = {class: 1}; if (ready) {{}}",
        )
        for prefix in prefixes:
            source = f'{prefix} /"Old.ttf"/.test(value); drawText("Player text");'
            with self.subTest(prefix=prefix):
                scan = scan_javascript(source)
                self.assertEqual([literal.value for literal in scan.literals], ["Player text"])
                self.assertNotIn("Old.ttf", scan.code)
                with self.assertRaises(ToolError):
                    replace_reviewed_javascript_literal(
                        source, line=1, source="Old.ttf", translation="New[.ttf", reviewed=True
                    )
        nested = '(function() { let value = 0; {} /"Old.ttf"/.test(value); drawText("Player text"); })();'
        self.assertEqual([literal.value for literal in scan_javascript(nested).literals], ["Player text"])

    def test_division_keeps_its_actual_string_operand(self) -> None:
        operands = (
            "value()",
            "((value))",
            "values[0]",
            "value++",
            "({value: 1})",
            "function() {}",
            "class {}",
            "(() => {})",
            "object.if(value)",
        )
        for operand in operands:
            source = f'const result = {operand} / "2" / denominator;'
            with self.subTest(operand=operand):
                result = replace_reviewed_javascript_literal(
                    source, line=1, source="2", translation="3", reviewed=True
                )
                self.assertEqual(result.text, source.replace('"2"', '"3"'))

    def test_font_rewrite_preserves_regex_and_updates_actual_loader(self) -> None:
        game = Path("D:/fixtures/game")
        source = 'if (ready) /"Old.ttf"/.test(value);\nGraphics.loadFont("GameFont", "Old.ttf");\n'
        patches, reviews = _scan_javascript(
            game / "js" / "main.js",
            source,
            game_root=game,
            content_root=game,
            assets=(FontAsset(game / "fonts" / "Old.ttf", "fonts/Old.ttf", 1, "old"),),
            aliases={},
            selected_name="Replacement[.ttf",
        )
        self.assertEqual(
            _apply_text_patches(source, patches),
            'if (ready) /"Old.ttf"/.test(value);\nGraphics.loadFont("GameFont", "Replacement%5B.ttf");\n',
        )
        self.assertFalse(reviews)


if __name__ == "__main__":
    unittest.main()
