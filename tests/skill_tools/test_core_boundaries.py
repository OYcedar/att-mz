from __future__ import annotations

import io
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Callable, Iterator, Mapping
from contextlib import redirect_stderr
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))

from att_skill_tools import (
    OutputPublishedError,
    ToolCancelledError,
    ToolError,
    atomic_write_bytes,
    atomic_write_directory,
    atomic_write_text,
    core,
    parse_json_text,
    physical_jsonl_lines,
    print_published_completion,
    protect_outputs,
    read_physical_text,
    safe_walk_files,
    write_json,
)


class _RacingFiles(Mapping[str, str]):
    def __init__(self, target: Path) -> None:
        self._values = {"report.json": "new\n"}
        self._target = target

    def __getitem__(self, key: str) -> str:
        return self._values[key]

    def __iter__(self) -> Iterator[str]:
        return iter(self._values)

    def __len__(self) -> int:
        return len(self._values)

    def items(self):
        self._target.mkdir()
        (self._target / "keep.txt").write_text("original\n", encoding="utf-8")
        return self._values.items()


class _InterruptedFiles(Mapping[str, str]):
    def __getitem__(self, key: str) -> str:
        raise KeyError(key)

    def __iter__(self) -> Iterator[str]:
        return iter(())

    def __len__(self) -> int:
        return 0

    def items(self):
        raise KeyboardInterrupt


class _FailedFiles(Mapping[str, str]):
    def __getitem__(self, key: str) -> str:
        raise KeyError(key)

    def __iter__(self) -> Iterator[str]:
        return iter(())

    def __len__(self) -> int:
        return 0

    def items(self):
        raise OSError("directory body failed")


class CoreBoundaryTests(unittest.TestCase):
    def test_atomic_bytes_uses_requested_fixed_suffix_and_preserves_exact_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "font.bin"
            candidate = target.with_name(".font.bin.att-font.tmp")
            body = b"\x00\xff\r\n\x80"

            atomic_write_bytes(
                target,
                body,
                replace=False,
                temporary_suffix=".att-font.tmp",
            )

            self.assertEqual(target.read_bytes(), body)
            self.assertFalse(candidate.exists())

    def test_atomic_text_preserves_exact_utf8_without_newline_translation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report.txt"
            text = "甲\r\n乙\n"

            atomic_write_text(target, text, replace=False)

            self.assertEqual(target.read_bytes(), text.encode("utf-8"))

    def test_atomic_bytes_preserves_committed_fact_with_custom_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "font.bin"
            candidate = target.with_name(".font.bin.att-font.tmp")
            real_link = os.link

            def publish_then_interrupt(source: Path, destination: Path) -> None:
                real_link(source, destination)
                raise KeyboardInterrupt

            with (
                patch.object(core.os, "link", side_effect=publish_then_interrupt),
                self.assertRaises(OutputPublishedError) as raised,
            ):
                atomic_write_bytes(
                    target,
                    b"font",
                    replace=False,
                    temporary_suffix=".att-font.tmp",
                )

            self.assertIsInstance(raised.exception.cause, KeyboardInterrupt)
            self.assertEqual(target.read_bytes(), b"font")
            self.assertFalse(candidate.exists())

    def test_file_publish_foreign_target_is_known_not_published_and_cleans_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report.bin"
            candidate = target.with_name(".report.bin.tmp")

            def occupy_target(_source: Path, _destination: Path) -> None:
                target.mkdir()
                raise FileExistsError("occupied")

            with (
                patch.object(core.os, "link", side_effect=occupy_target),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_bytes(target, b"new", replace=False)

            self.assertTrue(target.is_dir())
            self.assertFalse(candidate.exists())
            self.assertNotIn("无法确认目标文件是否已经发布", raised.exception.impact)

    def test_file_publish_probe_interrupt_exits_130_and_names_both_sites(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report.bin"
            candidate = target.with_name(".report.bin.tmp")
            real_identity = core._ordinary_file_identity  # pyright: ignore[reportPrivateUsage]
            publish_attempted = False
            stderr = io.StringIO()

            def fail_publish(_source: Path, _destination: Path) -> None:
                nonlocal publish_attempted
                publish_attempted = True
                raise PermissionError("publish blocked")

            def interrupt_target_probe(path: Path):
                if publish_attempted and path == target:
                    raise KeyboardInterrupt("target probe cancelled")
                return real_identity(path)

            def command() -> int:
                atomic_write_bytes(target, b"new", replace=False)
                return 0

            with (
                patch.object(core.os, "link", side_effect=fail_publish),
                patch.object(core, "_ordinary_file_identity", side_effect=interrupt_target_probe),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue(candidate.is_file())
            self.assertIn(str(target), stderr.getvalue())
            self.assertIn(str(candidate), stderr.getvalue())

    def test_file_write_failure_reports_cleanup_failure_or_cancellation_and_residual(self) -> None:
        cases = (
            (PermissionError("cleanup blocked"), 1),
            (KeyboardInterrupt("cleanup cancelled"), 130),
        )
        for cleanup_failure, expected_status in cases:
            with (
                self.subTest(cleanup=type(cleanup_failure).__name__),
                tempfile.TemporaryDirectory() as temporary,
            ):
                target = Path(temporary) / "report.bin"
                candidate = target.with_name(".report.bin.tmp")
                real_unlink = Path.unlink
                stderr = io.StringIO()

                def fail_fsync(_file_descriptor: int) -> None:
                    raise OSError("candidate fsync failed")

                def fail_candidate_cleanup(
                    path: Path,
                    missing_ok: bool = False,
                    failure: BaseException = cleanup_failure,
                    candidate_path: Path = candidate,
                    unlink: Callable[..., None] = real_unlink,
                ) -> None:
                    if path == candidate_path:
                        raise failure
                    unlink(path, missing_ok=missing_ok)

                def command(output: Path = target) -> int:
                    atomic_write_bytes(output, b"new", replace=False)
                    return 0

                with (
                    patch.object(core.os, "fsync", side_effect=fail_fsync),
                    patch.object(Path, "unlink", new=fail_candidate_cleanup),
                    redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as raised,
                ):
                    core.run_cli(command)

                self.assertEqual(raised.exception.code, expected_status)
                self.assertTrue(candidate.is_file())
                self.assertFalse(target.exists())
                self.assertIn("candidate fsync failed", stderr.getvalue())
                self.assertIn(str(candidate), stderr.getvalue())

    def test_completion_output_failure_keeps_completed_impact_and_exit_semantics(self) -> None:
        cases = (
            (OSError("stdout blocked"), ToolError, 1),
            (KeyboardInterrupt(), ToolCancelledError, 130),
        )
        for failure, expected_error, expected_status in cases:
            with self.subTest(failure=type(failure).__name__):
                stderr = io.StringIO()

                with (
                    patch("builtins.print", side_effect=failure),
                    self.assertRaises(expected_error) as completion,
                ):
                    print_published_completion(
                        "完成",
                        object_name="结果文件",
                        impact="结果文件已经完整发布",
                        help_text="直接打开结果文件继续处理",
                    )

                def command() -> int:
                    raise completion.exception

                with (
                    redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as raised,
                ):
                    core.run_cli(command)

                self.assertEqual(raised.exception.code, expected_status)
                self.assertIn("结果文件已经完整发布", stderr.getvalue())
                self.assertIn("最终完成提示输出失败", stderr.getvalue())

    def test_closed_stdout_reports_published_impact_and_exit_one(self) -> None:
        stdout = io.StringIO()
        stdout.close()
        stderr = io.StringIO()

        def command() -> int:
            print_published_completion(
                "完成",
                object_name="结果文件",
                impact="结果文件已经完整发布",
                help_text="直接打开结果文件继续处理",
            )
            return 0

        with (
            patch.object(sys, "stdout", stdout),
            redirect_stderr(stderr),
            self.assertRaises(SystemExit) as raised,
        ):
            core.run_cli(command)

        self.assertEqual(raised.exception.code, 1)
        self.assertIn("结果文件已经完整发布", stderr.getvalue())
        self.assertIn("最终完成提示输出失败", stderr.getvalue())
        self.assertIn("ValueError", stderr.getvalue())

    def test_run_cli_preserves_custom_os_error_detail_without_strerror(self) -> None:
        stderr = io.StringIO()

        def command() -> int:
            raise OSError("precise filesystem fact")

        with redirect_stderr(stderr), self.assertRaises(SystemExit) as raised:
            core.run_cli(command)

        self.assertEqual(raised.exception.code, 1)
        self.assertIn("precise filesystem fact", stderr.getvalue())

    def test_atomic_text_publish_interrupt_preserves_committed_file_and_exit_130(self) -> None:
        for replace in (False, True):
            with self.subTest(replace=replace), tempfile.TemporaryDirectory() as temporary:
                target = Path(temporary) / "report.json"
                if replace:
                    target.write_text("old\n", encoding="utf-8")
                operation = core.os.replace if replace else core.os.link
                stderr = io.StringIO()

                def publish_then_interrupt(
                    source: Path,
                    destination: Path,
                    publish: Callable[[Path, Path], None] = operation,
                ) -> None:
                    publish(source, destination)
                    raise KeyboardInterrupt

                def command(
                    output: Path = target,
                    replace_output: bool = replace,
                ) -> int:
                    atomic_write_text(output, "new\n", replace=replace_output)
                    return 0

                with (
                    patch.object(
                        core.os,
                        "replace" if replace else "link",
                        side_effect=publish_then_interrupt,
                    ),
                    redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as raised,
                ):
                    core.run_cli(command)

                self.assertEqual(raised.exception.code, 130)
                self.assertEqual(target.read_text(encoding="utf-8"), "new\n")
                self.assertFalse(target.with_name(".report.json.tmp").exists())
                self.assertIn("目标文件已经发布", stderr.getvalue())

    def test_atomic_text_unlink_interrupt_after_removal_keeps_published_fact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report.json"
            candidate = target.with_name(".report.json.tmp")
            real_unlink = Path.unlink
            stderr = io.StringIO()

            def unlink_then_interrupt(path: Path, missing_ok: bool = False) -> None:
                real_unlink(path, missing_ok=missing_ok)
                if path == candidate:
                    raise KeyboardInterrupt

            def command() -> int:
                atomic_write_text(target, "new\n", replace=False)
                return 0

            with (
                patch.object(Path, "unlink", new=unlink_then_interrupt),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertEqual(target.read_text(encoding="utf-8"), "new\n")
            self.assertFalse(candidate.exists())
            self.assertIn("固定临时文件已经清理", stderr.getvalue())

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

    def test_directory_publish_foreign_file_is_known_not_moved_and_cleans_stage(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            real_rename = os.rename

            def occupy_target(source: Path, destination: Path) -> None:
                if Path(source) == stage and Path(destination) == target:
                    target.write_bytes(b"foreign")
                    raise PermissionError("publish blocked")
                real_rename(source, destination)

            with (
                patch.object(core.os, "rename", side_effect=occupy_target),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=False)

            self.assertEqual(target.read_bytes(), b"foreign")
            self.assertFalse(stage.exists())
            self.assertNotIn("无法确认目录交换结果", raised.exception.impact)
            self.assertIn("不是普通目录", raised.exception.reason)

    def test_directory_publish_probe_interrupt_exits_130_and_names_both_sites(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
            real_rename = os.rename
            publish_attempted = False
            stderr = io.StringIO()

            def fail_publish(source: Path, destination: Path) -> None:
                nonlocal publish_attempted
                if Path(source) == stage and Path(destination) == target:
                    publish_attempted = True
                    raise PermissionError("publish blocked")
                real_rename(source, destination)

            def interrupt_target_probe(path: Path):
                if publish_attempted and path == target:
                    raise KeyboardInterrupt("target probe cancelled")
                return real_identity(path)

            def command() -> int:
                atomic_write_directory(target, {"report.json": "new\n"}, replace=False)
                return 0

            with (
                patch.object(core.os, "rename", side_effect=fail_publish),
                patch.object(core, "_ordinary_directory_identity", side_effect=interrupt_target_probe),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue(stage.is_dir())
            self.assertIn(str(target), stderr.getvalue())
            self.assertIn(str(stage), stderr.getvalue())

    def test_directory_stage_identity_error_cleans_candidate_and_raises_tool_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
            calls = 0

            def fail_first_identity(path: Path):
                nonlocal calls
                calls += 1
                if path == stage and calls == 1:
                    raise OSError("identity unavailable")
                return real_identity(path)

            with (
                patch.object(core, "_ordinary_directory_identity", side_effect=fail_first_identity),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=False)

            self.assertIn("identity unavailable", raised.exception.reason)
            self.assertIn("固定临时目录已经清理", raised.exception.impact)
            self.assertFalse(target.exists())
            self.assertFalse(stage.exists())

    def test_old_target_move_error_after_move_restores_and_reports_known_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            previous = target.with_name(".report.previous")
            stage = target.with_name(".report.tmp")
            target.mkdir()
            (target / "old.txt").write_text("old\n", encoding="utf-8")
            real_replace = os.replace

            def move_then_fail(source: Path, destination: Path) -> None:
                real_replace(source, destination)
                if Path(source) == target and Path(destination) == previous:
                    raise PermissionError("old move returned failure")

            with (
                patch.object(core.os, "replace", side_effect=move_then_fail),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertIn("old move returned failure", raised.exception.reason)
            self.assertIn("原目标仍位于目标路径", raised.exception.impact)
            self.assertEqual((target / "old.txt").read_text(encoding="utf-8"), "old\n")
            self.assertFalse(previous.exists())
            self.assertFalse(stage.exists())

    def test_new_publish_failure_rolls_back_and_reports_known_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            previous = target.with_name(".report.previous")
            stage = target.with_name(".report.tmp")
            target.mkdir()
            (target / "old.txt").write_text("old\n", encoding="utf-8")
            real_replace = os.replace

            def fail_new_publish(source: Path, destination: Path) -> None:
                if Path(source) == stage and Path(destination) == target:
                    raise PermissionError("new publish blocked")
                real_replace(source, destination)

            with (
                patch.object(core.os, "replace", side_effect=fail_new_publish),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertIn("new publish blocked", raised.exception.reason)
            self.assertIn("原目标仍位于目标路径", raised.exception.impact)
            self.assertEqual((target / "old.txt").read_text(encoding="utf-8"), "old\n")
            self.assertFalse(previous.exists())
            self.assertFalse(stage.exists())

    def test_directory_publish_reports_committed_after_rename_interrupt(self) -> None:
        for replace in (False, True):
            with self.subTest(replace=replace), tempfile.TemporaryDirectory() as temporary:
                target = Path(temporary) / "report"
                if replace:
                    target.mkdir()
                    (target / "old.txt").write_text("old\n", encoding="utf-8")
                real_move = os.replace if replace else os.rename

                stage = target.with_name(".report.tmp")

                def rename_then_interrupt(
                    source: Path,
                    destination: Path,
                    move: Callable[[Path, Path], None] = real_move,
                    expected_stage: Path = stage,
                ) -> None:
                    move(source, destination)
                    if Path(source) == expected_stage:
                        raise KeyboardInterrupt

                with (
                    patch(
                        f"att_skill_tools.core.os.{'replace' if replace else 'rename'}",
                        side_effect=rename_then_interrupt,
                    ),
                    self.assertRaises(OutputPublishedError) as raised,
                ):
                    atomic_write_directory(target, {"report.json": "new\n"}, replace=replace)

                self.assertIsInstance(raised.exception.cause, KeyboardInterrupt)
                self.assertEqual((target / "report.json").read_text(encoding="utf-8"), "new\n")
                self.assertFalse((target / "old.txt").exists())
                self.assertFalse(target.with_name(".report.tmp").exists())
                self.assertFalse(target.with_name(".report.previous").exists())

    def test_published_directory_probe_interrupt_exits_130_and_lists_fixed_site(self) -> None:
        for probe_name in ("previous", "cleanup"):
            with self.subTest(probe=probe_name), tempfile.TemporaryDirectory() as temporary:
                target = Path(temporary) / "report"
                stage = target.with_name(".report.tmp")
                previous = target.with_name(".report.previous")
                previous_cleanup = target.with_name(".report.previous.cleanup")
                probe_path = previous if probe_name == "previous" else previous_cleanup
                target.mkdir()
                (target / "old.txt").write_text("old\n", encoding="utf-8")
                real_replace = os.replace
                real_rmtree = core.shutil.rmtree
                real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
                cleanup_state = {"finished": False}
                stderr = io.StringIO()

                def publish_then_fail(
                    source: Path,
                    destination: Path,
                    move: Callable[[Path, Path], None] = real_replace,
                    expected_stage: Path = stage,
                    expected_target: Path = target,
                ) -> None:
                    move(source, destination)
                    if Path(source) == expected_stage and Path(destination) == expected_target:
                        raise PermissionError("publish returned failure")

                def track_old_cleanup(
                    path: Path,
                    remove_tree: Callable[[Path], None] = real_rmtree,
                    expected_cleanup: Path = previous_cleanup,
                    state: dict[str, bool] = cleanup_state,
                ) -> None:
                    remove_tree(path)
                    if Path(path) == expected_cleanup:
                        state["finished"] = True

                def interrupt_published_probe(
                    path: Path,
                    state: dict[str, bool] = cleanup_state,
                    expected_probe: Path = probe_path,
                    name: str = probe_name,
                    identity: Callable[[Path], tuple[int, int]] = real_identity,
                ):
                    if state["finished"] and path == expected_probe:
                        raise KeyboardInterrupt(f"{name} probe cancelled")
                    return identity(path)

                def command(output: Path = target) -> int:
                    atomic_write_directory(output, {"report.json": "new\n"}, replace=True)
                    return 0

                with (
                    patch.object(core.os, "replace", side_effect=publish_then_fail),
                    patch.object(core.shutil, "rmtree", side_effect=track_old_cleanup),
                    patch.object(
                        core,
                        "_ordinary_directory_identity",
                        side_effect=interrupt_published_probe,
                    ),
                    redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as raised,
                ):
                    core.run_cli(command)

                self.assertEqual(raised.exception.code, 130)
                self.assertEqual((target / "report.json").read_text(encoding="utf-8"), "new\n")
                self.assertIn("已经生效", stderr.getvalue())
                self.assertIn(str(probe_path), stderr.getvalue())
                self.assertIn(f"{probe_name} probe cancelled", stderr.getvalue())

    def test_directory_stage_identity_interrupt_cleans_candidate_and_exits_130(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
            calls = 0
            stderr = io.StringIO()

            def interrupt_first_identity(path: Path):
                nonlocal calls
                calls += 1
                if path == stage and calls == 1:
                    raise KeyboardInterrupt
                return real_identity(path)

            def command() -> int:
                atomic_write_directory(target, {"report.json": "new\n"}, replace=False)
                return 0

            with (
                patch.object(core, "_ordinary_directory_identity", side_effect=interrupt_first_identity),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertFalse(target.exists())
            self.assertFalse(stage.exists())
            self.assertFalse(target.with_name(".report.tmp.cleanup").exists())
            self.assertIn("固定临时目录已经清理", stderr.getvalue())

    def test_stage_setup_probe_interrupt_exits_130_and_reports_retained_stage(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            real_mkdir = Path.mkdir
            real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
            stage_created = False
            stderr = io.StringIO()

            def create_stage_then_fail(path: Path, *args: object, **kwargs: object) -> None:
                nonlocal stage_created
                real_mkdir(path, *args, **kwargs)  # pyright: ignore[reportArgumentType]
                if path == stage:
                    stage_created = True
                    raise PermissionError("stage setup failed")

            def interrupt_retained_probe(path: Path):
                if stage_created and path == stage:
                    raise KeyboardInterrupt("stage probe cancelled")
                return real_identity(path)

            def command() -> int:
                atomic_write_directory(target, {"report.json": "new\n"}, replace=False)
                return 0

            with (
                patch.object(Path, "mkdir", new=create_stage_then_fail),
                patch.object(core, "_ordinary_directory_identity", side_effect=interrupt_retained_probe),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue(stage.is_dir())
            self.assertIn("stage setup failed", stderr.getvalue())
            self.assertIn(str(stage), stderr.getvalue())
            self.assertIn("stage probe cancelled", stderr.getvalue())

    def test_directory_metadata_interrupt_cleans_candidate_and_exits_130(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            real_exists = Path.exists
            stderr = io.StringIO()

            def interrupt_after_stage_created(path: Path) -> bool:
                if path == target and real_exists(stage):
                    raise KeyboardInterrupt
                return real_exists(path)

            def command() -> int:
                atomic_write_directory(target, {"report.json": "new\n"}, replace=False)
                return 0

            with (
                patch.object(Path, "exists", new=interrupt_after_stage_created),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertFalse(target.exists())
            self.assertFalse(stage.exists())
            self.assertIn("固定临时目录已经清理", stderr.getvalue())

    def test_directory_cleanup_failure_reports_actual_cleanup_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            stage_cleanup = target.with_name(".report.tmp.cleanup")
            real_rename = core.os.rename

            def retain_claimed_cleanup(source: Path, destination: Path) -> None:
                if Path(source) == stage_cleanup and Path(destination) == stage:
                    raise PermissionError("restore blocked")
                real_rename(source, destination)

            with (
                patch.object(core.shutil, "rmtree", side_effect=PermissionError("cleanup blocked")),
                patch.object(core.os, "rename", side_effect=retain_claimed_cleanup),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_directory(target, _InterruptedFiles(), replace=False)

            self.assertFalse(stage.exists())
            self.assertTrue(stage_cleanup.exists())
            self.assertIn(str(stage_cleanup), raised.exception.impact)
            self.assertNotIn(f"{stage} 与", raised.exception.impact)

    def test_directory_publish_preserves_source_reoccupied_after_rename(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            real_rename = os.rename

            def rename_then_reoccupy(source: Path, destination: Path) -> None:
                real_rename(source, destination)
                if Path(source) == stage and Path(destination) == target:
                    stage.mkdir()
                    (stage / "foreign.txt").write_text("foreign\n", encoding="utf-8")
                    raise PermissionError("rename returned after publication")

            with (
                patch.object(core.os, "rename", side_effect=rename_then_reoccupy),
                self.assertRaises(OutputPublishedError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=False)

            self.assertEqual((target / "report.json").read_text(encoding="utf-8"), "new\n")
            self.assertEqual((stage / "foreign.txt").read_text(encoding="utf-8"), "foreign\n")
            self.assertIn(str(stage), raised.exception.reason)
            self.assertIn("已经生效", raised.exception.impact)

    def test_move_state_does_not_accept_expected_identity_at_both_sites(self) -> None:
        expected = (1, 2)
        with patch.object(
            core,
            "_directory_identity_at",
            side_effect=((expected, None), (expected, None)),
        ):
            state, facts, cancellation = core._directory_move_state(  # pyright: ignore[reportPrivateUsage]
                Path(".report.tmp"),
                Path("report"),
                expected,
            )

        self.assertEqual(state, "unknown")
        self.assertIn("同时具有预期身份", facts[0])
        self.assertIsNone(cancellation)

    def test_cancelled_directory_write_with_failed_cleanup_exits_130_and_reports_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            stage_cleanup = target.with_name(".report.tmp.cleanup")
            stderr = io.StringIO()

            def command() -> int:
                atomic_write_directory(target, _InterruptedFiles(), replace=False)
                return 0

            with (
                patch.object(core.shutil, "rmtree", side_effect=PermissionError("cleanup blocked")),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertIn("使用者取消了命令", stderr.getvalue())
            self.assertIn("临时目录清理也失败", stderr.getvalue())
            self.assertTrue(stage.is_dir())
            self.assertFalse(stage_cleanup.exists())

    def test_cleanup_path_probe_interrupt_exits_130_and_names_exact_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            stage_cleanup = target.with_name(".report.tmp.cleanup")
            real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
            interrupted = False
            stderr = io.StringIO()

            def interrupt_first_cleanup_probe(path: Path):
                nonlocal interrupted
                if path == stage_cleanup and not interrupted:
                    interrupted = True
                    raise KeyboardInterrupt("cleanup path probe cancelled")
                return real_identity(path)

            def command() -> int:
                atomic_write_directory(target, _FailedFiles(), replace=False)
                return 0

            with (
                patch.object(
                    core,
                    "_ordinary_directory_identity",
                    side_effect=interrupt_first_cleanup_probe,
                ),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue(stage.is_dir())
            self.assertFalse(stage_cleanup.exists())
            self.assertIn("directory body failed", stderr.getvalue())
            self.assertIn(str(stage_cleanup), stderr.getvalue())
            self.assertIn("cleanup path probe cancelled", stderr.getvalue())

    def test_cleanup_postcheck_error_reports_unknown_state_instead_of_identity_change(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owned = root / ".report.tmp"
            cleanup = root / ".report.tmp.cleanup"
            owned.mkdir()
            expected = core._ordinary_directory_identity(owned)  # pyright: ignore[reportPrivateUsage]
            real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
            cleanup_attempted = False

            def fail_cleanup(_path: Path) -> None:
                nonlocal cleanup_attempted
                cleanup_attempted = True
                raise PermissionError("cleanup blocked")

            def fail_postcheck(path: Path):
                if cleanup_attempted and path == cleanup:
                    raise OSError("cleanup postcheck blocked")
                return real_identity(path)

            with (
                patch.object(core.shutil, "rmtree", side_effect=fail_cleanup),
                patch.object(core, "_ordinary_directory_identity", side_effect=fail_postcheck),
            ):
                error = core.remove_owned_directory(owned, expected, cleanup)

            self.assertIsInstance(error, OSError)
            self.assertNotIsInstance(error, KeyboardInterrupt)
            self.assertIn("后验状态无法确认", str(error))
            self.assertIn("cleanup postcheck blocked", str(error))
            self.assertIn(str(cleanup), str(error))
            self.assertNotIn("身份已经变化", str(error))
            self.assertFalse(owned.exists())
            self.assertTrue(cleanup.is_dir())

    def test_cleanup_postcheck_interrupt_exits_130_after_primary_os_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage_cleanup = target.with_name(".report.tmp.cleanup")
            real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
            cleanup_attempted = False
            stderr = io.StringIO()

            def fail_cleanup(_path: Path) -> None:
                nonlocal cleanup_attempted
                cleanup_attempted = True
                raise PermissionError("cleanup blocked")

            def interrupt_postcheck(path: Path):
                if cleanup_attempted and path == stage_cleanup:
                    raise KeyboardInterrupt("cleanup postcheck cancelled")
                return real_identity(path)

            def command() -> int:
                atomic_write_directory(target, _FailedFiles(), replace=False)
                return 0

            with (
                patch.object(core.shutil, "rmtree", side_effect=fail_cleanup),
                patch.object(core, "_ordinary_directory_identity", side_effect=interrupt_postcheck),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertIn("directory body failed", stderr.getvalue())
            self.assertIn(str(stage_cleanup), stderr.getvalue())
            self.assertIn("cleanup postcheck cancelled", stderr.getvalue())

    def test_cleanup_failure_retained_probe_interrupt_exits_130(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            stage = target.with_name(".report.tmp")
            stage_cleanup = target.with_name(".report.tmp.cleanup")
            real_identity = core._ordinary_directory_identity  # pyright: ignore[reportPrivateUsage]
            real_rename = os.rename
            restored = False
            restored_stage_probes = 0
            stderr = io.StringIO()

            def fail_cleanup(_path: Path) -> None:
                raise PermissionError("cleanup blocked")

            def track_restore(source: Path, destination: Path) -> None:
                nonlocal restored
                real_rename(source, destination)
                if Path(source) == stage_cleanup and Path(destination) == stage:
                    restored = True

            def interrupt_second_restored_probe(path: Path):
                nonlocal restored_stage_probes
                if restored and path == stage:
                    restored_stage_probes += 1
                    if restored_stage_probes == 2:
                        raise KeyboardInterrupt("retained stage probe cancelled")
                return real_identity(path)

            def command() -> int:
                atomic_write_directory(target, _FailedFiles(), replace=False)
                return 0

            with (
                patch.object(core.shutil, "rmtree", side_effect=fail_cleanup),
                patch.object(core.os, "rename", side_effect=track_restore),
                patch.object(
                    core,
                    "_ordinary_directory_identity",
                    side_effect=interrupt_second_restored_probe,
                ),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                core.run_cli(command)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue(stage.is_dir())
            self.assertFalse(stage_cleanup.exists())
            self.assertIn("cleanup blocked", stderr.getvalue())
            self.assertIn(str(stage), stderr.getvalue())
            self.assertIn("retained stage probe cancelled", stderr.getvalue())

    def test_interrupt_before_cleanup_claim_remains_keyboard_interrupt(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owned = root / ".report.tmp"
            cleanup = root / ".report.tmp.cleanup"
            owned.mkdir()
            expected = core._ordinary_directory_identity(owned)  # pyright: ignore[reportPrivateUsage]

            with patch.object(core.os, "rename", side_effect=KeyboardInterrupt()):
                error = core.remove_owned_directory(owned, expected, cleanup)

            self.assertIsInstance(error, KeyboardInterrupt)
            self.assertTrue(owned.is_dir())
            self.assertFalse(cleanup.exists())

    def test_unknown_cleanup_claim_interrupt_reports_both_fixed_sites(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owned = root / ".report.tmp"
            cleanup = root / ".report.tmp.cleanup"
            owned.mkdir()
            expected = core._ordinary_directory_identity(owned)  # pyright: ignore[reportPrivateUsage]

            with (
                patch.object(core.os, "rename", side_effect=KeyboardInterrupt()),
                patch.object(
                    core,
                    "_directory_move_state",
                    return_value=("unknown", ("源目录状态无法读取",), KeyboardInterrupt()),
                ),
            ):
                error = core.remove_owned_directory(owned, expected, cleanup)

            self.assertIsInstance(error, KeyboardInterrupt)
            self.assertIn(str(owned), str(error))
            self.assertIn(str(cleanup), str(error))

    def test_failed_previous_cleanup_restore_reports_only_actual_residual_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            previous = target.with_name(".report.previous")
            previous_cleanup = target.with_name(".report.previous.cleanup")
            target.mkdir()
            (target / "old.txt").write_text("old\n", encoding="utf-8")
            real_rename = core.os.rename

            def fail_cleanup_restore(source: Path, destination: Path) -> None:
                if Path(source) == previous_cleanup and Path(destination) == previous:
                    raise PermissionError("restore blocked")
                real_rename(source, destination)

            with (
                patch.object(core.shutil, "rmtree", side_effect=PermissionError("cleanup blocked")),
                patch.object(core.os, "rename", side_effect=fail_cleanup_restore),
                self.assertRaises(OutputPublishedError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertEqual((target / "report.json").read_text(encoding="utf-8"), "new\n")
            self.assertFalse(previous.exists())
            self.assertTrue(previous_cleanup.is_dir())
            self.assertIn(str(previous_cleanup), raised.exception.impact)
            self.assertNotIn(f"{previous} 与", raised.exception.impact)

    def test_replace_interrupt_after_old_target_move_restores_old_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            previous = target.with_name(".report.previous")
            stage = target.with_name(".report.tmp")
            target.mkdir()
            (target / "old.txt").write_text("old\n", encoding="utf-8")
            real_replace = os.replace

            def interrupt_after_old_move(source: Path, destination: Path) -> None:
                real_replace(source, destination)
                if Path(source) == target and Path(destination) == previous:
                    raise KeyboardInterrupt

            with (
                patch("att_skill_tools.core.os.replace", side_effect=interrupt_after_old_move),
                self.assertRaises(ToolCancelledError),
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertEqual((target / "old.txt").read_text(encoding="utf-8"), "old\n")
            self.assertFalse((target / "report.json").exists())
            self.assertFalse(stage.exists())
            self.assertFalse(previous.exists())

    def test_replace_preserves_target_reoccupied_after_old_target_move(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            previous = target.with_name(".report.previous")
            stage = target.with_name(".report.tmp")
            target.mkdir()
            (target / "old.txt").write_text("old\n", encoding="utf-8")
            real_replace = os.replace

            def move_then_reoccupy(source: Path, destination: Path) -> None:
                real_replace(source, destination)
                if Path(source) == target and Path(destination) == previous:
                    target.mkdir()
                    (target / "foreign.txt").write_text("foreign\n", encoding="utf-8")
                    raise PermissionError("move returned after replacement")

            with (
                patch.object(core.os, "replace", side_effect=move_then_reoccupy),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertIn(f"旧目录仍位于 {previous}", raised.exception.impact)
            self.assertEqual((target / "foreign.txt").read_text(encoding="utf-8"), "foreign\n")
            self.assertEqual((previous / "old.txt").read_text(encoding="utf-8"), "old\n")
            self.assertEqual((stage / "report.json").read_text(encoding="utf-8"), "new\n")

    def test_replace_interrupt_after_successful_rollback_reports_cancelled_restored_state(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            previous = target.with_name(".report.previous")
            stage = target.with_name(".report.tmp")
            target.mkdir()
            (target / "old.txt").write_text("old\n", encoding="utf-8")
            real_replace = os.replace

            def fail_publish_then_interrupt_rollback(source: Path, destination: Path) -> None:
                if Path(source) == stage and Path(destination) == target:
                    raise PermissionError("publish blocked")
                real_replace(source, destination)
                if Path(source) == previous and Path(destination) == target:
                    raise KeyboardInterrupt

            with (
                patch(
                    "att_skill_tools.core.os.replace",
                    side_effect=fail_publish_then_interrupt_rollback,
                ),
                self.assertRaises(ToolCancelledError),
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertEqual((target / "old.txt").read_text(encoding="utf-8"), "old\n")
            self.assertFalse((target / "report.json").exists())
            self.assertFalse(stage.exists())
            self.assertFalse(previous.exists())

    def test_replace_preserves_foreign_target_instead_of_rolling_back_over_it(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            previous = target.with_name(".report.previous")
            stage = target.with_name(".report.tmp")
            target.mkdir()
            (target / "old.txt").write_text("old\n", encoding="utf-8")
            real_replace = os.replace

            def inject_foreign_target(source: Path, destination: Path) -> None:
                if Path(source) == stage and Path(destination) == target:
                    target.mkdir()
                    (target / "foreign.txt").write_text("foreign\n", encoding="utf-8")
                    raise PermissionError("publish blocked")
                real_replace(source, destination)

            with (
                patch("att_skill_tools.core.os.replace", side_effect=inject_foreign_target),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertIn(f"旧目录仍位于 {previous}", raised.exception.impact)
            self.assertEqual((target / "foreign.txt").read_text(encoding="utf-8"), "foreign\n")
            self.assertEqual((previous / "old.txt").read_text(encoding="utf-8"), "old\n")
            self.assertEqual((stage / "report.json").read_text(encoding="utf-8"), "new\n")

    def test_directory_identity_rejects_zero_inode(self) -> None:
        metadata = SimpleNamespace(
            st_mode=stat.S_IFDIR | 0o755,
            st_file_attributes=0,
            st_dev=0,
            st_ino=0,
        )
        with (
            patch.object(Path, "lstat", return_value=metadata),
            self.assertRaises(OSError) as raised,
        ):
            core._ordinary_directory_identity(Path("report"))  # pyright: ignore[reportPrivateUsage]

        self.assertIn("稳定目录身份", str(raised.exception))

    def test_owned_directory_cleanup_preserves_replacement_at_claim_boundary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owned = root / ".report.tmp"
            cleanup = root / ".report.tmp.cleanup"
            displaced = root / "owned-displaced"
            replacement = root / "replacement"
            owned.mkdir()
            (owned / "owned.txt").write_text("owned\n", encoding="utf-8")
            expected = core._ordinary_directory_identity(owned)  # pyright: ignore[reportPrivateUsage]
            replacement.mkdir()
            (replacement / "foreign.txt").write_text("foreign\n", encoding="utf-8")
            real_rename = os.rename
            raced = False

            def replace_before_claim(source: Path, destination: Path) -> None:
                nonlocal raced
                if Path(source) == owned and not raced:
                    raced = True
                    real_rename(owned, displaced)
                    real_rename(replacement, owned)
                real_rename(source, destination)

            with patch.object(core.os, "rename", side_effect=replace_before_claim):
                error = core.remove_owned_directory(owned, expected, cleanup)

            self.assertIsInstance(error, OSError)
            self.assertEqual((owned / "foreign.txt").read_text(encoding="utf-8"), "foreign\n")
            self.assertEqual((displaced / "owned.txt").read_text(encoding="utf-8"), "owned\n")
            self.assertFalse(cleanup.exists())

    def test_publish_success_is_verified_against_stage_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "report"
            displaced = root / "published-displaced"
            real_rename = os.rename

            def replace_after_publish(source: Path, destination: Path) -> None:
                real_rename(source, destination)
                if Path(destination) == target:
                    real_rename(target, displaced)
                    target.mkdir()
                    (target / "foreign.txt").write_text("foreign\n", encoding="utf-8")

            with (
                patch.object(core.os, "rename", side_effect=replace_after_publish),
                self.assertRaises(ToolError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=False)

            self.assertIn("无法确认目录交换结果", raised.exception.impact)
            self.assertEqual((target / "foreign.txt").read_text(encoding="utf-8"), "foreign\n")
            self.assertEqual((displaced / "report.json").read_text(encoding="utf-8"), "new\n")

    def test_previous_cleanup_failure_is_reported_as_committed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "report"
            previous = target.with_name(".report.previous")
            target.mkdir()
            (target / "old.txt").write_text("old\n", encoding="utf-8")
            real_rmtree = core.shutil.rmtree
            previous_cleanup = target.with_name(".report.previous.cleanup")

            def fail_previous_cleanup(path: Path) -> None:
                if Path(path) == previous_cleanup:
                    raise PermissionError("cleanup blocked")
                real_rmtree(path)

            with (
                patch.object(core.shutil, "rmtree", side_effect=fail_previous_cleanup),
                self.assertRaises(OutputPublishedError) as raised,
            ):
                atomic_write_directory(target, {"report.json": "new\n"}, replace=True)

            self.assertIsInstance(raised.exception.cause, PermissionError)
            self.assertEqual((target / "report.json").read_text(encoding="utf-8"), "new\n")
            self.assertEqual((previous / "old.txt").read_text(encoding="utf-8"), "old\n")

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

    def test_physical_text_reader_does_not_normalize_lone_cr(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "records.jsonl"
            path.write_bytes(b"{}\r{}\n")

            with self.assertRaises(ToolError):
                list(physical_jsonl_lines(read_physical_text(path), str(path)))

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
