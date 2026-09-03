from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from collections.abc import ItemsView
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))

from att_skill_tools import (
    ToolError,
    atomic_write_directory,
    atomic_write_text,
    parse_json_text,
    physical_jsonl_lines,
    protect_outputs,
    safe_walk_files,
    write_json,
)


class _RacingFiles(dict[str, str]):
    def __init__(self, target: Path) -> None:
        super().__init__({"report.json": "new\n"})
        self._target = target

    def items(self) -> ItemsView[str, str]:
        self._target.mkdir()
        (self._target / "keep.txt").write_text("original\n", encoding="utf-8")
        return super().items()


class CoreBoundaryTests(unittest.TestCase):
    def test_protect_outputs_rejects_every_overlap_direction(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            protected = root / "game"
            protected.mkdir()
            input_root = root / "manual"
            input_root.mkdir()

            cases = (
                ("forbidden same", protected, (), (protected,)),
                ("forbidden child", protected / "report", (), (protected,)),
                ("forbidden ancestor", root, (), (protected,)),
                ("input same", input_root, (input_root,), ()),
                ("input child", input_root / "report", (input_root,), ()),
                ("input ancestor", root, (input_root,), ()),
            )
            for name, output, inputs, forbidden_roots in cases:
                with self.subTest(name=name), self.assertRaises(ToolError):
                    protect_outputs(
                        [output],
                        inputs=inputs,
                        forbidden_roots=forbidden_roots,
                        replace=True,
                    )

    def test_output_parent_link_is_rejected_without_touching_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            victim = root / "victim"
            victim.mkdir()
            marker = victim / "keep.txt"
            marker.write_text("original\n", encoding="utf-8")
            link = root / "output-link"
            self._make_directory_link(link, victim)
            try:
                with self.assertRaises(ToolError):
                    protect_outputs([link / "report"], replace=True)
                with self.assertRaises(ToolError):
                    atomic_write_text(link / "report.json", "new\n", replace=True)
                with self.assertRaises(ToolError):
                    atomic_write_directory(link, {"report.json": "new\n"}, replace=True)
                self.assertEqual(marker.read_text(encoding="utf-8"), "original\n")
                self.assertFalse((victim / "report.json").exists())
            finally:
                if os.name == "nt":
                    link.rmdir()
                else:
                    link.unlink()

    def test_create_new_directory_race_preserves_later_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"

            with self.assertRaises(ToolError) as raised:
                atomic_write_directory(target, _RacingFiles(target), replace=False)

            self.assertIn("写入期间已由其他进程建立", raised.exception.reason)
            self.assertEqual((target / "keep.txt").read_text(encoding="utf-8"), "original\n")
            self.assertFalse((target / "report.json").exists())
            self.assertFalse(target.with_name(".report.tmp").exists())
            self.assertFalse(target.with_name(".report.previous").exists())

    def test_directory_publish_never_replaces_an_existing_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            target.write_text("original\n", encoding="utf-8")

            with self.assertRaises(ToolError):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertEqual(target.read_text(encoding="utf-8"), "original\n")
            self.assertFalse(target.with_name(".report.tmp").exists())
            self.assertFalse(target.with_name(".report.previous").exists())

    def test_safe_walk_rejects_hard_link_before_yield(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source.json"
            alias = root / "alias.json"
            source.write_text("{}\n", encoding="utf-8")
            try:
                os.link(source, alias)
            except OSError as error:
                self.skipTest(f"当前文件系统无法建立硬链接：{error}")

            yielded: list[Path] = []
            with self.assertRaises(ToolError) as raised:
                yielded.extend(safe_walk_files(root))

            self.assertEqual(yielded, [])
            self.assertIn("硬链接", raised.exception.reason)

    def test_json_number_overflow_is_rejected_before_serialization(self) -> None:
        with self.assertRaises(ToolError) as raised:
            parse_json_text('{"value":1e999}', "测试 JSON")

        self.assertIn("有限范围", raised.exception.reason)

        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.json"
            with self.assertRaises(ToolError) as write_error:
                write_json(output, {"value": float("inf")}, replace=False)
            self.assertIn("非有限数字", write_error.exception.reason)
            self.assertFalse(output.exists())

    def test_physical_jsonl_lines_preserve_unicode_separators_and_line_numbers(self) -> None:
        text = '{"text":"甲\u2028乙\u0085丙"}\r\n{"text":"丁"}\n'

        self.assertEqual(
            list(physical_jsonl_lines(text, "records.jsonl")),
            [(1, '{"text":"甲\u2028乙\u0085丙"}'), (2, '{"text":"丁"}')],
        )
        self.assertEqual(list(physical_jsonl_lines("", "records.jsonl")), [])

    def test_physical_jsonl_lines_reject_lone_cr_at_natural_line(self) -> None:
        cases = (("middle", "{}\n{\r}\n", 2), ("end", "{}\r", 1))
        for name, text, line_number in cases:
            with self.subTest(name=name), self.assertRaises(ToolError) as raised:
                list(physical_jsonl_lines(text, "records.jsonl"))

            self.assertEqual(raised.exception.object_name, "records.jsonl")
            self.assertIn(f"第 {line_number} 行", raised.exception.reason)

    def _make_directory_link(self, link: Path, target: Path) -> None:
        if os.name != "nt":
            os.symlink(target, link, target_is_directory=True)
            return
        result = subprocess.run(
            ["cmd.exe", "/d", "/c", "mklink", "/J", str(link), str(target)],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            self.skipTest("当前 Windows 文件系统无法建立目录 junction")


if __name__ == "__main__":
    unittest.main()
