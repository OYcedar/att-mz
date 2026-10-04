from __future__ import annotations

import json
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT_ROOT = ROOT / "skills" / "translate-with-att" / "scripts"
sys.path.insert(0, str(SCRIPT_ROOT))

from att_toolbox.font_metadata import check_font_coverage


def _write_font(path: Path) -> None:
    # cmap 12 覆盖 BMP 可见字符，特意缺少 U+0301 和补充平面汉字。
    groups = struct.pack(">IIIIII", 0x20, 0x0300, 1, 0x0302, 0xFFFD, 0x0300)
    subtable = struct.pack(">HHIII", 12, 0, 16 + len(groups), 0, 2) + groups
    cmap = struct.pack(">HHHHI", 0, 1, 3, 10, 12) + subtable
    header = struct.pack(">IHHHH", 0x00010000, 1, 16, 0, 0)
    table = struct.pack(">4sIII", b"cmap", 0, 28, len(cmap))
    path.write_bytes(header + table + cmap)


class FontCoverageTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.font = self.root / "Replacement.ttf"
        _write_font(self.font)

    def test_attached_selectors_require_only_the_base_glyph(self) -> None:
        for text in ("‼", "‼\ufe0e", "‼\ufe0f", "一\ufe00", "一\U000e0100", "ᠠ\u180b", "ᠠ\u180f"):
            with self.subTest(text=text):
                coverage = check_font_coverage(self.font, extra_characters=text)
                self.assertEqual(coverage.missing_characters, "")
                self.assertEqual(coverage.unattached_variation_selectors, "")
                self.assertTrue(set(text[:1]).issubset(coverage.checked_characters))
                if len(text) > 1:
                    self.assertNotIn(text[1:], coverage.checked_characters)

    def test_missing_bases_and_ordinary_combining_marks_remain_missing(self) -> None:
        coverage = check_font_coverage(self.font, extra_characters="𠀀\U000e0100a\u0301")
        self.assertEqual(coverage.missing_characters, "\u0301𠀀")
        self.assertEqual(coverage.unattached_variation_selectors, "")

    def test_misplaced_selectors_are_text_diagnostics_even_when_cmap_maps_them(self) -> None:
        for text, expected in (
            ("\ufe0e", "\ufe0e"),
            ("A \ufe0e", "\ufe0e"),
            ("A\n\U000e0100", "\U000e0100"),
            ("A\u200d\ufe0e", "\ufe0e"),
            ("a\u0301\ufe0e", "\ufe0e"),
            ("‼\ufe0e\ufe0f", "\ufe0f"),
        ):
            with self.subTest(text=text):
                coverage = check_font_coverage(self.font, extra_characters=text)
                self.assertEqual(coverage.unattached_variation_selectors, expected)
                self.assertNotIn(expected, coverage.missing_characters)

    def test_extra_text_files_keep_their_own_sequence_boundaries(self) -> None:
        first = self.root / "first.txt"
        second = self.root / "second.txt"
        first.write_text("A", encoding="utf-8")
        second.write_text("\ufeff\ufe0e", encoding="utf-8")
        coverage = check_font_coverage(self.font, (first, second), extra_characters="B")
        self.assertEqual(coverage.missing_characters, "")
        self.assertEqual(coverage.unattached_variation_selectors, "\ufe0e")

    def test_inspect_reports_write_back_projection_and_file_boundaries(self) -> None:
        game = self.root / "game"
        (game / "js").mkdir(parents=True)
        (game / "data").mkdir()
        (game / "fonts").mkdir()
        (game / "js/rmmz_core.js").write_text("// MZ\n", encoding="utf-8")
        (game / "js/plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
        (game / "data/System.json").write_text("{}", encoding="utf-8")
        (game / "fonts/Old.ttf").write_bytes(b"old-font")
        (game / "style.css").write_text(
            "@font-face{font-family:GameFont;src:url(fonts/Old.ttf)}body{font-family:GameFont}",
            encoding="utf-8",
        )
        (game / "index.html").write_text(
            '<link rel="stylesheet" href="style.css"><script src="js/rmmz_core.js"></script>'
            '<script src="js/plugins.js"></script>',
            encoding="utf-8",
        )
        before = {path.relative_to(game): path.read_bytes() for path in game.rglob("*") if path.is_file()}
        translations = self.root / "translations.jsonl"
        output = self.root / "report.json"
        first = self.root / "first.txt"
        second = self.root / "second.txt"
        for translated, pending, additions, missing, unattached in (
            (["‼\ufe0e"], "一\U000e0100", ("‼\ufe0f", "中"), "", ""),
            (["𠀀\U000e0100", "a\u0301"], "中", ("‼\ufe0f", "中"), "\u0301𠀀", ""),
            (["A", "\ufe0e"], "\ufe0f", ("B", "\U000e0100"), "", "\ufe0e\ufe0f\U000e0100"),
            (["A"], "中", ("\ufe0e", "‼\ufe0f"), "", "\ufe0e"),
        ):
            with self.subTest(translated=translated, pending=pending, additions=additions):
                rows = [
                    {
                        "manual_id": "CommonEvents.json:1:list:1",
                        "source": ["原文"] * len(translated),
                        "translation": translated,
                        "type": "free",
                        "state": "current",
                        "origin": "manual",
                    },
                    {
                        "manual_id": "CommonEvents.json:1:list:2",
                        "source": [pending],
                        "translation": None,
                        "type": "free",
                        "state": "pending",
                        "origin": "none",
                    },
                    {
                        "manual_id": "CommonEvents.json:1:list:3",
                        "source": ["中"],
                        "translation": None,
                        "type": "free",
                        "state": "rejected",
                        "origin": "automatic",
                        "rejected_candidate_json": '["𠀁"]',
                    },
                ]
                translations.write_text(
                    "\n".join(json.dumps(row, ensure_ascii=False) for row in rows), encoding="utf-8"
                )
                first.write_text(additions[0], encoding="utf-8")
                second.write_text(additions[1], encoding="utf-8")
                result = subprocess.run(
                    [
                        sys.executable,
                        "-B",
                        str(SCRIPT_ROOT / "manage_rpg_maker_fonts.py"),
                        "inspect",
                        "--game",
                        str(game),
                        "--font",
                        str(self.font),
                        "--translations",
                        str(translations),
                        "--coverage-text",
                        str(first),
                        "--coverage-text",
                        str(second),
                        "--output",
                        str(output),
                        "--replace",
                    ],
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                report = json.loads(output.read_text(encoding="utf-8"))
                self.assertEqual(report["coverage"]["missing_characters"], missing)
                self.assertEqual(report["coverage"]["unattached_variation_selectors"], unattached)
                self.assertEqual(report["coverage"]["project_missing_characters"], missing)
                self.assertEqual(report["coverage"]["additional_missing_characters"], "")
                for field in (
                    "checked_characters",
                    "project_checked_characters",
                    "additional_checked_characters",
                ):
                    self.assertFalse(set("\ufe0e\ufe0f\U000e0100") & set(report["coverage"][field]))
                reasons = {item["reason"] for item in report["review"]}
                self.assertEqual("selected_font_missing_checked_characters" in reasons, bool(missing))
                self.assertEqual("unattached_variation_selector" in reasons, bool(unattached))
                self.assertEqual(report["review_required"], bool(missing or unattached))
                self.assertEqual(
                    report["qa_status"], "needs_review" if missing or unattached else "unverified"
                )
        after = {path.relative_to(game): path.read_bytes() for path in game.rglob("*") if path.is_file()}
        self.assertEqual(after, before)


if __name__ == "__main__":
    unittest.main()
