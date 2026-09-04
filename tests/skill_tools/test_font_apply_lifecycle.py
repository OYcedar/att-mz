from __future__ import annotations

import argparse
import builtins
import gc
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from collections.abc import Callable, Sequence
from contextlib import ExitStack
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from typing import Self, TypeAlias
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

import att_toolbox.font_transaction as transaction
import manage_rpg_maker_fonts as fonts
from att_skill_tools import (
    JsonValue,
    OutputPublishedError,
    ToolCancelledError,
    ToolError,
    atomic_write_directory,
    core,
)
from att_toolbox.font_metadata import FontCoverage
from att_toolbox.font_references import FontPlan
from att_toolbox.font_transaction import (
    ByteMutation,
    FontStateBinding,
    apply_font_plan,
    bind_font_state,
    font_state_files,
)

_RestoreStateBindingForTest: TypeAlias = (
    transaction._RestoreStateBinding  # pyright: ignore[reportPrivateUsage]
)
_RestoreEntryForTest: TypeAlias = transaction._RestoreEntry  # pyright: ignore[reportPrivateUsage]


class FontApplyLifecycleTests(unittest.TestCase):
    def _plan(self, root: Path, names: tuple[str, ...] = ("fonts/main.otf",)) -> FontPlan:
        game = (root / "game").resolve()
        selected = root / "selected.otf"
        game.mkdir()
        selected.write_bytes(b"replacement-font")
        mutations: list[ByteMutation] = []
        for number, name in enumerate(names, start=1):
            original = f"old-{number}".encode()
            target = game / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(original)
            mutations.append(ByteMutation(name, original, f"new-{number}".encode()))
        coverage = FontCoverage(glyph_count=2, checked_characters="字体", missing_characters="")
        return FontPlan(
            game_root=game,
            content_root=game,
            selected_font=selected,
            selected_sha256=hashlib.sha256(selected.read_bytes()).hexdigest(),
            selected_size=selected.stat().st_size,
            assets=(),
            aliases=(),
            references=(),
            reviews=(),
            mutations=tuple(mutations),
            coverage=coverage,
        )

    def _coverage(self) -> SimpleNamespace:
        return SimpleNamespace(
            additional_paths=(),
            translation_path=None,
            translation_identity=None,
            project_text="",
            additional_text="",
        )

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

    def _publish_and_bind(self, plan: FontPlan, state: Path) -> FontStateBinding:
        atomic_write_directory(state, font_state_files(plan), replace=False)
        return bind_font_state(plan, state=state)

    def _replace_state(self, plan: FontPlan, state: Path, displaced: Path) -> None:
        state.rename(displaced)
        atomic_write_directory(state, font_state_files(plan), replace=False)

    def test_cancel_after_state_publish_reports_game_unchanged(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            state = root / "font-state"
            output = root / "font-report.json"
            plan = SimpleNamespace(
                game_root=root / "game",
                selected_font=root / "font.otf",
                mutations=(object(),),
            )
            coverage = SimpleNamespace(additional_paths=(), translation_path=None)
            published = OutputPublishedError(
                object_name=str(state),
                reason="目录已经发布，但发布调用返回前发生：使用者取消了命令",
                impact=f"新目录 {state} 已经生效",
                help_text="保留已经发布的目标",
                cause=KeyboardInterrupt(),
            )
            arguments = argparse.Namespace(output=output, state=state, replace=False)

            with (
                patch.object(fonts, "_coverage_projection", return_value=coverage),
                patch.object(fonts, "_game_root", return_value=plan.game_root),
                patch.object(fonts, "_plan", return_value=plan),
                patch.object(fonts, "protect_outputs"),
                patch.object(fonts, "verify_font_plan_source"),
                patch.object(fonts, "font_state_files", return_value={"manifest.json": "{}\n"}),
                patch.object(fonts, "atomic_write_directory", side_effect=published),
                patch.object(fonts, "apply_font_plan") as apply,
                self.assertRaises(ToolCancelledError) as raised,
            ):
                fonts._run_apply(arguments)  # pyright: ignore[reportPrivateUsage]

            apply.assert_not_called()
            self.assertIsInstance(raised.exception.cause, KeyboardInterrupt)
            self.assertIn("state 已经完整建立", raised.exception.impact)
            self.assertIn("目标游戏尚未开始字体替换", raised.exception.impact)
            self.assertIn("执行 restore", raised.exception.help_text)

    def test_apply_rejects_same_content_state_with_new_directory_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            displaced = root / "displaced-state"
            binding = self._publish_and_bind(plan, state)
            self._replace_state(plan, state, displaced)

            with self.assertRaises(ToolError) as raised:
                apply_font_plan(plan, state=state, binding=binding)

            self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"old-1")
            self.assertIn("目录身份", raised.exception.reason)
            self.assertIn("尚未开始字体写入", raised.exception.impact)
            self.assertIn("残留位置无法确认", raised.exception.impact)
            self.assertTrue((displaced / "manifest.json").is_file())

    def test_apply_rejects_tampered_manifest_and_snapshot_before_game_write(self) -> None:
        for relative in ("manifest.json", "before/000001.bin", "after/000001.bin"):
            with self.subTest(relative=relative), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                plan = self._plan(root)
                state = root / "font-state"
                binding = self._publish_and_bind(plan, state)
                (state / relative).write_bytes(b"tampered")

                with self.assertRaises(ToolError) as raised:
                    apply_font_plan(plan, state=state, binding=binding)

                self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"old-1")
                self.assertIn(relative, raised.exception.reason)
                self.assertIn("尚未开始字体写入", raised.exception.impact)
                self.assertIn("已被修改", raised.exception.impact)

    def test_state_replacement_during_apply_rolls_back_game_from_memory_plan(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root, ("fonts/one.otf", "fonts/two.otf"))
            state = root / "font-state"
            displaced = root / "displaced-state"
            binding = self._publish_and_bind(plan, state)
            original_write = transaction._atomic_write_bytes  # pyright: ignore[reportPrivateUsage]
            replaced = False

            def write_then_replace(target: Path, body: bytes, *, expect_missing: bool) -> None:
                nonlocal replaced
                original_write(target, body, expect_missing=expect_missing)
                if not replaced and target == plan.game_root / "fonts/one.otf":
                    replaced = True
                    self._replace_state(plan, state, displaced)

            with (
                patch.object(transaction, "_atomic_write_bytes", side_effect=write_then_replace),
                self.assertRaises(ToolError) as raised,
            ):
                apply_font_plan(plan, state=state, binding=binding)

            self.assertEqual((plan.game_root / "fonts/one.otf").read_bytes(), b"old-1")
            self.assertEqual((plan.game_root / "fonts/two.otf").read_bytes(), b"old-2")
            self.assertIn("现已恢复为 apply 前字节", raised.exception.impact)
            self.assertIn("残留位置无法确认", raised.exception.impact)
            self.assertEqual(
                (displaced / "status.json").read_text(encoding="utf-8"), '{\n  "status": "prepared"\n}\n'
            )

    def test_manifest_tamper_after_game_write_is_detected_before_applied_status(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            binding = self._publish_and_bind(plan, state)
            original_write = transaction._atomic_write_bytes  # pyright: ignore[reportPrivateUsage]
            tampered = False

            def write_then_tamper(target: Path, body: bytes, *, expect_missing: bool) -> None:
                nonlocal tampered
                original_write(target, body, expect_missing=expect_missing)
                if not tampered and target == plan.game_root / "fonts/main.otf":
                    tampered = True
                    (state / "manifest.json").write_bytes(b"tampered")

            with (
                patch.object(transaction, "_atomic_write_bytes", side_effect=write_then_tamper),
                self.assertRaises(ToolError) as raised,
            ):
                apply_font_plan(plan, state=state, binding=binding)

            self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"old-1")
            self.assertIn("manifest.json", raised.exception.reason)
            self.assertIn("现已恢复为 apply 前字节", raised.exception.impact)
            self.assertIn("不能作为恢复依据", raised.exception.impact)

    def test_state_replacement_before_marker_reports_game_applied_and_no_report(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            displaced = root / "displaced-state"
            output = root / "report.json"
            arguments = argparse.Namespace(output=output, state=state, replace=False)
            original_marker = fonts._write_apply_marker  # pyright: ignore[reportPrivateUsage]

            def replace_then_mark(
                marker_plan: FontPlan,
                marker_state: Path,
                binding: FontStateBinding,
                report: dict[str, JsonValue],
            ) -> bytes:
                self._replace_state(plan, state, displaced)
                return original_marker(marker_plan, marker_state, binding, report)

            with (
                patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                patch.object(fonts, "_game_root", return_value=plan.game_root),
                patch.object(fonts, "_plan", return_value=plan),
                patch.object(fonts, "protect_outputs"),
                patch.object(fonts, "_write_apply_marker", side_effect=replace_then_mark),
                self.assertRaises(ToolError) as raised,
            ):
                fonts._run_apply(arguments)  # pyright: ignore[reportPrivateUsage]

            self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"new-1")
            self.assertFalse((state / "applied.json").exists())
            self.assertFalse(output.exists())
            self.assertIn("字体替换已完整生效", raised.exception.impact)
            self.assertIn("Review JSON 尚未发布", raised.exception.impact)
            self.assertIn("残留位置无法确认", raised.exception.impact)

    def test_state_replacement_after_marker_write_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            displaced = root / "displaced-state"
            output = root / "report.json"
            arguments = argparse.Namespace(output=output, state=state, replace=False)
            original_write = transaction._atomic_write_bytes  # pyright: ignore[reportPrivateUsage]
            replaced = False

            def write_then_replace(target: Path, body: bytes, *, expect_missing: bool) -> None:
                nonlocal replaced
                original_write(target, body, expect_missing=expect_missing)
                if not replaced and target == state / "applied.json":
                    replaced = True
                    self._replace_state(plan, state, displaced)

            with (
                patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                patch.object(fonts, "_game_root", return_value=plan.game_root),
                patch.object(fonts, "_plan", return_value=plan),
                patch.object(fonts, "protect_outputs"),
                patch.object(transaction, "_atomic_write_bytes", side_effect=write_then_replace),
                self.assertRaises(ToolError) as raised,
            ):
                fonts._run_apply(arguments)  # pyright: ignore[reportPrivateUsage]

            self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"new-1")
            self.assertTrue((displaced / "applied.json").is_file())
            self.assertFalse(output.exists())
            self.assertIn("字体替换已完整生效", raised.exception.impact)
            self.assertIn("Review JSON 尚未发布", raised.exception.impact)
            self.assertIn("残留位置无法确认", raised.exception.impact)

    def test_published_apply_marker_cleanup_failure_keeps_exact_facts_through_cli(self) -> None:
        failures: tuple[tuple[BaseException, type[ToolError], int], ...] = (
            (PermissionError("marker temporary cleanup blocked"), OutputPublishedError, 1),
            (KeyboardInterrupt("marker temporary cleanup cancelled"), ToolCancelledError, 130),
        )
        for cleanup_failure, expected_type, expected_code in failures:
            with (
                self.subTest(failure=type(cleanup_failure).__name__),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                plan = self._plan(root)
                state = root / "font-state"
                output = root / "report.json"
                arguments = argparse.Namespace(output=output, state=state, replace=False)
                marker = state / "applied.json"
                marker_temporary = state / ".applied.json.att-font.tmp"
                path_type = type(marker)
                real_unlink = path_type.unlink

                def fail_marker_temporary_cleanup(
                    path: Path,
                    missing_ok: bool = False,
                    *,
                    current_temporary: Path = marker_temporary,
                    current_failure: BaseException = cleanup_failure,
                    unlink: Callable[..., None] = real_unlink,
                ) -> None:
                    if path == current_temporary:
                        raise current_failure
                    unlink(path, missing_ok=missing_ok)

                with (
                    patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                    patch.object(fonts, "_game_root", return_value=plan.game_root),
                    patch.object(fonts, "_plan", return_value=plan),
                    patch.object(path_type, "unlink", new=fail_marker_temporary_cleanup),
                    patch("att_skill_tools.core.print_error") as rendered,
                    self.assertRaises(SystemExit) as raised,
                ):
                    fonts.run_cli(
                        lambda current_arguments=arguments: fonts._run_apply(  # pyright: ignore[reportPrivateUsage]
                            current_arguments
                        )
                    )

                self.assertEqual(raised.exception.code, expected_code)
                rendered.assert_called_once()
                reported = rendered.call_args.args[0]
                self.assertIsInstance(reported, expected_type)
                self.assertIs(reported.cause, cleanup_failure)
                self.assertIn("目标文件已经发布", reported.reason)
                self.assertIn(str(marker), reported.impact)
                self.assertIn(str(marker_temporary), reported.impact)
                self.assertNotIn("applied 标记或 Review JSON 未完整发布", reported.impact)
                self.assertEqual(json.loads(marker.read_text(encoding="utf-8"))["applied"], True)
                self.assertEqual(marker_temporary.read_bytes(), marker.read_bytes())
                self.assertEqual(
                    json.loads((state / "status.json").read_text(encoding="utf-8")),
                    {"status": "applied"},
                )
                self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"new-1")
                self.assertFalse(output.exists())
                lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
                self.assertFalse(lock.exists())
                self.assertFalse(cleanup.exists())

    def test_same_game_tasks_share_one_fixed_directory_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            first_entered = threading.Event()
            finish_first = threading.Event()
            failures: list[BaseException] = []

            def first_operation() -> int:
                first_entered.set()
                if not finish_first.wait(timeout=5):
                    raise AssertionError("等待第二个字体任务超时")
                return 0

            def run_first() -> None:
                try:
                    fonts._run_with_font_game_lock(  # pyright: ignore[reportPrivateUsage]
                        plan.game_root,
                        first_operation,
                    )
                except BaseException as error:  # noqa: BLE001 - 测试线程必须回传实际失败。
                    failures.append(error)

            worker = threading.Thread(target=run_first)
            worker.start()
            self.assertTrue(first_entered.wait(timeout=5))
            second_called = False

            def second_operation() -> int:
                nonlocal second_called
                second_called = True
                return 0

            try:
                with self.assertRaises(ToolError) as raised:
                    fonts._run_with_font_game_lock(  # pyright: ignore[reportPrivateUsage]
                        plan.game_root,
                        second_operation,
                    )
                self.assertIn("已有字体 apply/restore 任务", raised.exception.reason)
                self.assertFalse(second_called)
            finally:
                finish_first.set()
                worker.join(timeout=5)
            self.assertFalse(worker.is_alive())
            self.assertEqual(failures, [])
            lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
            self.assertFalse(lock.exists())
            self.assertFalse(cleanup.exists())

    def test_lock_mkdir_cancellation_survives_each_failed_presence_probe_through_cli(self) -> None:
        probe_failures: tuple[BaseException, ...] = (
            ToolError(
                object_name="lock probe",
                reason="锁路径状态无法读取",
                impact="状态未知",
                help_text="检查路径",
            ),
            OSError("lock lstat failed"),
            KeyboardInterrupt("lock probe cancelled"),
        )
        for probe_failure in probe_failures:
            with (
                self.subTest(failure=type(probe_failure).__name__),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                plan = self._plan(root)
                lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
                path_type = type(lock)
                real_mkdir = path_type.mkdir
                real_path_present = transaction._path_present  # pyright: ignore[reportPrivateUsage]
                original_cancellation = KeyboardInterrupt("lock mkdir cancelled")

                def mkdir_then_cancel(
                    path: Path,
                    mode: int = 0o777,
                    parents: bool = False,
                    exist_ok: bool = False,
                    *,
                    mkdir: Callable[..., None] = real_mkdir,
                    current_lock: Path = lock,
                    cancellation: KeyboardInterrupt = original_cancellation,
                ) -> None:
                    mkdir(path, mode=mode, parents=parents, exist_ok=exist_ok)
                    if path == current_lock:
                        raise cancellation

                def fail_lock_probe(
                    path: Path,
                    *,
                    current_lock: Path = lock,
                    failure: BaseException = probe_failure,
                    path_present: Callable[[Path], bool] = real_path_present,
                ) -> bool:
                    if path == current_lock:
                        raise failure
                    return path_present(path)

                try:
                    with (
                        patch.object(path_type, "mkdir", new=mkdir_then_cancel),
                        patch.object(transaction, "_path_present", side_effect=fail_lock_probe),
                        patch("att_skill_tools.core.print_error") as rendered,
                        self.assertRaises(SystemExit) as raised,
                    ):
                        fonts.run_cli(
                            lambda plan_root=plan.game_root: fonts._run_with_font_game_lock(  # pyright: ignore[reportPrivateUsage]
                                plan_root,
                                lambda: 0,
                            )
                        )

                    self.assertEqual(raised.exception.code, 130)
                    rendered.assert_called_once()
                    reported = rendered.call_args.args[0]
                    self.assertIsInstance(reported, ToolCancelledError)
                    self.assertIs(reported.cause, original_cancellation)
                    self.assertIn(str(lock), reported.impact)
                    self.assertIn("任务锁需确认", reported.impact)
                    self.assertTrue(lock.is_dir())
                    self.assertFalse(cleanup.exists())
                finally:
                    if lock.exists():
                        lock.rmdir()

    def test_inspect_and_apply_build_plan_only_while_game_lock_is_held(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = replace(self._plan(root), mutations=())
            observations: list[bool] = []

            def build_locked_plan(
                _arguments: argparse.Namespace,
                _coverage: object,
            ) -> FontPlan:
                lock, _cleanup = transaction.font_game_lock_paths(plan.game_root)
                observations.append(lock.is_dir())
                return plan

            inspect_arguments = argparse.Namespace(output=root / "inspect.json", replace=False)
            apply_arguments = argparse.Namespace(
                output=root / "apply.json",
                state=root / "unused-state",
                replace=False,
            )
            with (
                patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                patch.object(fonts, "_game_root", return_value=plan.game_root),
                patch.object(fonts, "_plan", side_effect=build_locked_plan),
            ):
                self.assertEqual(fonts._run_inspect(inspect_arguments), 0)  # pyright: ignore[reportPrivateUsage]
                self.assertEqual(fonts._run_apply(apply_arguments), 0)  # pyright: ignore[reportPrivateUsage]

            self.assertEqual(observations, [True, True])

    def test_apply_does_not_scan_a_mixed_game_while_restore_holds_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root, ("fonts/one.otf", "fonts/two.otf"))
            (plan.game_root / "fonts/two.otf").write_bytes(b"new-2")
            arguments = argparse.Namespace(
                output=root / "apply.json",
                state=root / "state",
                replace=False,
            )
            held = transaction.acquire_font_game_lock(plan.game_root)
            try:
                with (
                    patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                    patch.object(fonts, "_game_root", return_value=plan.game_root),
                    patch.object(fonts, "_plan", return_value=plan) as build,
                    self.assertRaises(ToolError),
                ):
                    fonts._run_apply(arguments)  # pyright: ignore[reportPrivateUsage]
                build.assert_not_called()
            finally:
                release = transaction.release_font_game_lock(held)
            self.assertEqual(release.errors, ())
            self.assertEqual(release.retained_sites, ())
            self.assertEqual(release.uncertain_sites, ())

    def test_inspect_cross_protects_output_temporary_and_game_lock_paths(self) -> None:
        for output_kind in ("lock", "cleanup", "retained_tmp"):
            with self.subTest(output_kind=output_kind), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                plan = self._plan(root)
                lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
                output = {"lock": lock, "cleanup": cleanup}.get(output_kind, root / "inspect.json")
                temporary_output = output.with_name(f".{output.name}.tmp")
                if output_kind == "retained_tmp":
                    temporary_output.write_bytes(b"retained")
                arguments = argparse.Namespace(output=output, replace=True)

                with (
                    patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                    patch.object(fonts, "_game_root", return_value=plan.game_root),
                    patch.object(fonts, "_plan", return_value=plan),
                    self.assertRaises(ToolError),
                ):
                    fonts._run_inspect(arguments)  # pyright: ignore[reportPrivateUsage]

                self.assertFalse(output.exists())
                self.assertFalse(lock.exists())
                self.assertFalse(cleanup.exists())
                if output_kind == "retained_tmp":
                    self.assertEqual(temporary_output.read_bytes(), b"retained")

    def test_lock_wrapper_preserves_cancellation_and_release_facts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)

            def cancel_operation() -> int:
                raise KeyboardInterrupt

            with self.assertRaises(ToolCancelledError) as cancelled:
                fonts._run_with_font_game_lock(  # pyright: ignore[reportPrivateUsage]
                    plan.game_root,
                    cancel_operation,
                )
            self.assertIsInstance(cancelled.exception.cause, KeyboardInterrupt)
            self.assertIn("取消点可核对的实际状态", cancelled.exception.impact)
            lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
            self.assertFalse(lock.exists())
            self.assertFalse(cleanup.exists())

            try:
                with (
                    patch.object(
                        fonts,
                        "release_font_game_lock",
                        return_value=transaction.FontGameLockRelease(
                            (KeyboardInterrupt(),),
                            (lock,),
                            (),
                        ),
                    ),
                    self.assertRaises(ToolCancelledError) as release_cancelled,
                ):
                    fonts._run_with_font_game_lock(  # pyright: ignore[reportPrivateUsage]
                        plan.game_root,
                        lambda: 0,
                    )
                self.assertIsInstance(release_cancelled.exception.cause, KeyboardInterrupt)
                self.assertIn("主流程已经完成", release_cancelled.exception.impact)
                self.assertIn(str(lock), release_cancelled.exception.impact)
                self.assertNotIn(str(cleanup), release_cancelled.exception.impact)
            finally:
                if lock.exists():
                    lock.rmdir()

    def test_lock_cleanup_interrupt_after_removal_reports_cleaned_and_exit_130(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary) / "game"
            game_root.mkdir()
            lock, cleanup = transaction.font_game_lock_paths(game_root)
            real_rmtree = core.shutil.rmtree

            def remove_then_interrupt(path: Path) -> None:
                real_rmtree(path)
                raise KeyboardInterrupt

            with (
                patch.object(core.shutil, "rmtree", side_effect=remove_then_interrupt),
                patch("att_skill_tools.core.print_error") as rendered,
                self.assertRaises(SystemExit) as raised,
            ):
                fonts.run_cli(
                    lambda: fonts._run_with_font_game_lock(  # pyright: ignore[reportPrivateUsage]
                        game_root,
                        lambda: 0,
                    )
                )

            self.assertEqual(raised.exception.code, 130)
            self.assertFalse(lock.exists())
            self.assertFalse(cleanup.exists())
            error = rendered.call_args.args[0]
            self.assertIsInstance(error, ToolCancelledError)
            self.assertIn("字体任务锁已经清理", error.impact)
            self.assertNotIn("需确认", error.impact)
            self.assertNotIn(str(lock), error.impact)
            self.assertNotIn(str(cleanup), error.impact)

    def test_lock_release_reports_retained_and_uncertain_sites_together(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            game_root = Path(temporary) / "game"
            game_root.mkdir()
            lock, cleanup = transaction.font_game_lock_paths(game_root)
            try:
                with (
                    patch.object(
                        fonts,
                        "release_font_game_lock",
                        return_value=transaction.FontGameLockRelease(
                            (OSError("release incomplete"),),
                            (lock,),
                            (cleanup,),
                        ),
                    ),
                    self.assertRaises(ToolError) as raised,
                ):
                    fonts._run_with_font_game_lock(  # pyright: ignore[reportPrivateUsage]
                        game_root,
                        lambda: 0,
                    )

                self.assertIn(f"任务锁现场保留于 {lock}", raised.exception.impact)
                self.assertIn(f"任务锁状态需确认于 {cleanup}", raised.exception.impact)
            finally:
                if lock.exists():
                    lock.rmdir()

    def test_restore_rechecks_each_existing_and_created_target_immediately(self) -> None:
        for created in (False, True):
            for late_position in ("before", "after", "third"):
                with (
                    self.subTest(created=created, late_position=late_position),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    root = Path(temporary)
                    plan = self._plan(root)
                    target = plan.game_root / "fonts/main.otf"
                    before = None if created else b"old-1"
                    if created:
                        target.unlink()
                        plan = replace(
                            plan,
                            mutations=(ByteMutation("fonts/main.otf", None, b"new-1"),),
                        )
                    state = root / "font-state"
                    binding = self._publish_and_bind(plan, state)
                    apply_font_plan(plan, state=state, binding=binding)
                    original_position = transaction._restore_target_position  # pyright: ignore[reportPrivateUsage]
                    reads = 0

                    def change_before_immediate_read(
                        current_target: Path,
                        current_before: bytes | None,
                        current_after: bytes,
                        *,
                        transition: str = late_position,
                        read_position: Callable[[Path, bytes | None, bytes], str] = original_position,
                    ) -> str:
                        nonlocal reads
                        reads += 1
                        if reads == 2:
                            if transition == "before":
                                if current_before is None:
                                    current_target.unlink()
                                else:
                                    current_target.write_bytes(current_before)
                            elif transition == "third":
                                current_target.write_bytes(b"third-party-change")
                        return read_position(current_target, current_before, current_after)

                    with patch.object(
                        transaction,
                        "_restore_target_position",
                        side_effect=change_before_immediate_read,
                    ):
                        if late_position == "third":
                            with self.assertRaises(ToolError):
                                transaction.restore_font_state(game_root=plan.game_root, state=state)
                        else:
                            restored = transaction.restore_font_state(game_root=plan.game_root, state=state)
                            self.assertEqual(restored, 1 if late_position == "after" else 0)

                    if late_position == "third":
                        self.assertEqual(target.read_bytes(), b"third-party-change")
                    elif before is None:
                        self.assertFalse(target.exists())
                    else:
                        self.assertEqual(target.read_bytes(), before)

    def test_restore_rejects_same_byte_link_path_for_created_and_replaced_targets(self) -> None:
        for created in (False, True):
            with self.subTest(created=created), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                plan = self._plan(root)
                target = plan.game_root / "fonts/main.otf"
                if created:
                    target.unlink()
                    plan = replace(
                        plan,
                        mutations=(ByteMutation("fonts/main.otf", None, b"new-1"),),
                    )
                state = root / "font-state"
                binding = self._publish_and_bind(plan, state)
                apply_font_plan(plan, state=state, binding=binding)
                referent_root = root / "same-byte-referent"
                referent_root.mkdir()
                referent = referent_root / "main.otf"
                referent.write_bytes(b"new-1")
                target.unlink()
                target.parent.rmdir()
                self._make_directory_link(target.parent, referent_root)

                try:
                    with self.assertRaises(ToolError) as raised:
                        transaction.restore_font_state(game_root=plan.game_root, state=state)

                    self.assertIn("符号链接", raised.exception.reason)
                    self.assertEqual(referent.read_bytes(), b"new-1")
                    self.assertEqual(target.read_bytes(), b"new-1")
                    self.assertEqual(
                        json.loads((state / "status.json").read_text(encoding="utf-8")),
                        {"status": "applied"},
                    )
                finally:
                    if os.name == "nt":
                        target.parent.rmdir()
                    else:
                        target.parent.unlink()

    def test_restore_initial_scan_cancellation_is_typed_before_any_write(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            binding = self._publish_and_bind(plan, state)
            apply_font_plan(plan, state=state, binding=binding)

            with (
                patch.object(transaction, "_restore_target_position", side_effect=KeyboardInterrupt),
                self.assertRaises(ToolCancelledError) as raised,
            ):
                transaction.restore_font_state(game_root=plan.game_root, state=state)

            self.assertIsInstance(raised.exception.cause, KeyboardInterrupt)
            self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"new-1")
            self.assertEqual(
                json.loads((state / "status.json").read_text(encoding="utf-8")), {"status": "applied"}
            )

    def test_restore_final_verification_cancellation_records_restored_state(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            binding = self._publish_and_bind(plan, state)
            apply_font_plan(plan, state=state, binding=binding)
            original_verify = transaction._verify_restored_targets  # pyright: ignore[reportPrivateUsage]
            verification_count = 0

            def cancel_first_verification(
                game_root: Path,
                current_state: Path,
                binding: _RestoreStateBindingForTest,
                entries: Sequence[_RestoreEntryForTest],
            ) -> None:
                nonlocal verification_count
                verification_count += 1
                if verification_count == 1:
                    raise KeyboardInterrupt
                original_verify(game_root, current_state, binding, entries)

            with (
                patch.object(
                    transaction,
                    "_verify_restored_targets",
                    side_effect=cancel_first_verification,
                ),
                self.assertRaises(ToolCancelledError) as raised,
            ):
                transaction.restore_font_state(game_root=plan.game_root, state=state)

            self.assertIsInstance(raised.exception.cause, KeyboardInterrupt)
            self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"old-1")
            self.assertEqual(
                json.loads((state / "status.json").read_text(encoding="utf-8")), {"status": "restored"}
            )

    def test_restore_final_verification_error_confirms_bytes_and_records_restored(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            binding = self._publish_and_bind(plan, state)
            apply_font_plan(plan, state=state, binding=binding)
            original_verify = transaction._verify_restored_targets  # pyright: ignore[reportPrivateUsage]
            verification_count = 0

            def fail_first_verification(
                game_root: Path,
                current_state: Path,
                current_binding: _RestoreStateBindingForTest,
                entries: Sequence[_RestoreEntryForTest],
            ) -> None:
                nonlocal verification_count
                verification_count += 1
                if verification_count == 1:
                    raise PermissionError("transient verification failure")
                original_verify(game_root, current_state, current_binding, entries)

            with (
                patch.object(
                    transaction,
                    "_verify_restored_targets",
                    side_effect=fail_first_verification,
                ),
                self.assertRaises(ToolError) as raised,
            ):
                transaction.restore_font_state(game_root=plan.game_root, state=state)

            self.assertNotIsInstance(raised.exception, ToolCancelledError)
            self.assertIn("已经恢复为 apply 前字节", raised.exception.impact)
            self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"old-1")
            self.assertEqual(
                json.loads((state / "status.json").read_text(encoding="utf-8")), {"status": "restored"}
            )

    def test_restore_final_verification_drift_records_recovery_required(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            binding = self._publish_and_bind(plan, state)
            apply_font_plan(plan, state=state, binding=binding)
            original_position = transaction._restore_target_position  # pyright: ignore[reportPrivateUsage]
            position_reads = 0

            def drift_at_final_verification(
                target: Path,
                before: bytes | None,
                after: bytes,
            ) -> str:
                nonlocal position_reads
                position_reads += 1
                if position_reads == 3:
                    target.write_bytes(b"late-third-party-change")
                return original_position(target, before, after)

            with (
                patch.object(
                    transaction,
                    "_restore_target_position",
                    side_effect=drift_at_final_verification,
                ),
                self.assertRaises(ToolError) as raised,
            ):
                transaction.restore_font_state(game_root=plan.game_root, state=state)

            self.assertIn("无法确认目标游戏状态", raised.exception.impact)
            self.assertIn("recovery_required", raised.exception.impact)
            self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"late-third-party-change")
            self.assertEqual(
                json.loads((state / "status.json").read_text(encoding="utf-8")),
                {"status": "recovery_required"},
            )

    def test_restore_streams_snapshot_bytes_one_entry_at_a_time(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            names = tuple(f"fonts/{index:02d}.otf" for index in range(12))
            plan = self._plan(root, names)
            state = root / "font-state"
            binding = self._publish_and_bind(plan, state)
            apply_font_plan(plan, state=state, binding=binding)
            original_blob = transaction._state_blob  # pyright: ignore[reportPrivateUsage]
            live = 0
            peak = 0

            class TrackedBytes(bytes):
                def __new__(cls, value: bytes) -> Self:
                    nonlocal live, peak
                    instance = super().__new__(cls, value)
                    live += 1
                    peak = max(peak, live)
                    return instance

                def __del__(self) -> None:
                    nonlocal live
                    live -= 1

            def tracked_blob(
                current_state: Path,
                current_binding: _RestoreStateBindingForTest,
                name: str,
                expected_sha: str,
            ) -> bytes:
                return TrackedBytes(original_blob(current_state, current_binding, name, expected_sha))

            with patch.object(transaction, "_state_blob", side_effect=tracked_blob):
                restored = transaction.restore_font_state(game_root=plan.game_root, state=state)
            gc.collect()

            self.assertEqual(restored, len(names))
            self.assertLessEqual(peak, 6)
            self.assertEqual(live, 0)

    def test_apply_atomic_publish_interrupt_uses_shared_state_machine_and_rolls_back(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            output = root / "apply.json"
            target = plan.game_root / "fonts/main.otf"
            arguments = argparse.Namespace(output=output, state=state, replace=False)
            real_replace = transaction.os.replace
            interrupted = False

            def interrupt_after_font_publish(source: Path, destination: Path) -> None:
                nonlocal interrupted
                real_replace(source, destination)
                if Path(destination) == target and not interrupted:
                    interrupted = True
                    raise KeyboardInterrupt

            with (
                patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                patch.object(fonts, "_game_root", return_value=plan.game_root),
                patch.object(fonts, "_plan", return_value=plan),
                patch("att_skill_tools.core.os.replace", side_effect=interrupt_after_font_publish),
                patch("att_skill_tools.core.print_error"),
                self.assertRaises(SystemExit) as raised,
            ):
                fonts.run_cli(lambda: fonts._run_apply(arguments))  # pyright: ignore[reportPrivateUsage]

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue(interrupted)
            self.assertEqual(target.read_bytes(), b"old-1")
            self.assertFalse(target.with_name(".main.otf.att-font.tmp").exists())
            self.assertEqual(
                json.loads((state / "status.json").read_text(encoding="utf-8")),
                {"status": "rolled_back"},
            )
            self.assertFalse(output.exists())

    def test_apply_atomic_cleanup_failure_preserves_exact_font_temporary_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            target = plan.game_root / "fonts/main.otf"
            target.unlink()
            plan = replace(
                plan,
                mutations=(ByteMutation("fonts/main.otf", None, b"new-1"),),
            )
            state = root / "font-state"
            binding = self._publish_and_bind(plan, state)
            font_temporary = target.with_name(".main.otf.att-font.tmp")
            path_type = type(target)
            real_unlink = path_type.unlink

            def block_font_temporary_unlink(path: Path, missing_ok: bool = False) -> None:
                if path == font_temporary:
                    raise PermissionError("temporary cleanup blocked")
                real_unlink(path, missing_ok=missing_ok)

            with (
                patch.object(path_type, "unlink", new=block_font_temporary_unlink),
                self.assertRaises(ToolError) as raised,
            ):
                apply_font_plan(plan, state=state, binding=binding)

            self.assertFalse(target.exists())
            self.assertEqual(font_temporary.read_bytes(), b"new-1")
            self.assertIn(str(font_temporary), raised.exception.impact)
            self.assertEqual(
                json.loads((state / "status.json").read_text(encoding="utf-8")),
                {"status": "rolled_back"},
            )

    def test_final_completion_output_failure_preserves_each_committed_command(self) -> None:
        failures: tuple[tuple[BaseException, int], ...] = (
            (PermissionError("terminal unavailable"), 1),
            (KeyboardInterrupt(), 130),
        )
        for command in ("inspect", "apply", "restore"):
            for failure, expected_code in failures:
                with (
                    self.subTest(command=command, failure=type(failure).__name__),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    root = Path(temporary)
                    plan = self._plan(root)
                    output = root / f"{command}.json"
                    state = root / "font-state"
                    target = plan.game_root / "fonts/main.otf"
                    contexts = ExitStack()
                    if command == "inspect":
                        arguments = argparse.Namespace(output=output, replace=False)
                        operation = lambda arguments=arguments: fonts._run_inspect(  # pyright: ignore[reportPrivateUsage]
                            arguments
                        )
                        expected_body = b"old-1"
                        expected_impact = "Review JSON"
                        contexts.enter_context(
                            patch.object(fonts, "_coverage_projection", return_value=self._coverage())
                        )
                        contexts.enter_context(patch.object(fonts, "_game_root", return_value=plan.game_root))
                        contexts.enter_context(patch.object(fonts, "_plan", return_value=plan))
                    elif command == "apply":
                        arguments = argparse.Namespace(output=output, state=state, replace=False)
                        operation = lambda arguments=arguments: fonts._run_apply(  # pyright: ignore[reportPrivateUsage]
                            arguments
                        )
                        expected_body = b"new-1"
                        expected_impact = "均已完整生效"
                        contexts.enter_context(
                            patch.object(fonts, "_coverage_projection", return_value=self._coverage())
                        )
                        contexts.enter_context(patch.object(fonts, "_game_root", return_value=plan.game_root))
                        contexts.enter_context(patch.object(fonts, "_plan", return_value=plan))
                    else:
                        binding = self._publish_and_bind(plan, state)
                        apply_font_plan(plan, state=state, binding=binding)
                        arguments = argparse.Namespace(
                            game=plan.game_root,
                            state=state,
                            output=output,
                            replace=False,
                        )
                        operation = lambda arguments=arguments: fonts._run_restore(  # pyright: ignore[reportPrivateUsage]
                            arguments
                        )
                        expected_body = b"old-1"
                        expected_impact = "已经恢复为 apply 前字节"
                        contexts.enter_context(
                            patch.object(fonts, "discover_game", return_value=SimpleNamespace())
                        )
                        contexts.enter_context(
                            patch.object(fonts, "require_game_root", return_value=plan.game_root)
                        )
                    with (
                        contexts,
                        patch.object(builtins, "print", side_effect=failure),
                        patch("att_skill_tools.core.print_error") as rendered,
                        self.assertRaises(SystemExit) as raised,
                    ):
                        fonts.run_cli(operation)

                    self.assertEqual(raised.exception.code, expected_code)
                    rendered.assert_called_once()
                    terminal_error = rendered.call_args.args[0]
                    self.assertIsInstance(terminal_error, ToolError)
                    self.assertIn(expected_impact, terminal_error.impact)
                    self.assertTrue(output.is_file())
                    self.assertEqual(target.read_bytes(), expected_body)
                    if command == "apply":
                        self.assertEqual(
                            json.loads((state / "status.json").read_text(encoding="utf-8")),
                            {"status": "applied"},
                        )
                    elif command == "restore":
                        self.assertEqual(
                            json.loads((state / "status.json").read_text(encoding="utf-8")),
                            {"status": "restored"},
                        )
                        self.assertTrue((state / "restored.json").is_file())

    def test_apply_cancellation_rolls_back_and_cli_returns_130(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root, ("fonts/one.otf", "fonts/two.otf"))
            state = root / "font-state"
            output = root / "apply.json"
            arguments = argparse.Namespace(output=output, state=state, replace=False)
            original_write = transaction._atomic_write_bytes  # pyright: ignore[reportPrivateUsage]

            def cancel_second_write(target: Path, body: bytes, *, expect_missing: bool) -> None:
                if target == plan.game_root / "fonts/two.otf" and body == b"new-2":
                    raise KeyboardInterrupt
                original_write(target, body, expect_missing=expect_missing)

            with (
                patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                patch.object(fonts, "_game_root", return_value=plan.game_root),
                patch.object(fonts, "_plan", return_value=plan),
                patch.object(transaction, "_atomic_write_bytes", side_effect=cancel_second_write),
                patch("att_skill_tools.core.print_error"),
                self.assertRaises(SystemExit) as raised,
            ):
                fonts.run_cli(lambda: fonts._run_apply(arguments))  # pyright: ignore[reportPrivateUsage]

            self.assertEqual(raised.exception.code, 130)
            self.assertEqual((plan.game_root / "fonts/one.otf").read_bytes(), b"old-1")
            self.assertEqual((plan.game_root / "fonts/two.otf").read_bytes(), b"old-2")
            self.assertEqual(
                json.loads((state / "status.json").read_text(encoding="utf-8")), {"status": "rolled_back"}
            )
            lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
            self.assertFalse(lock.exists())
            self.assertFalse(cleanup.exists())

    def test_restore_cancellation_rolls_back_and_cli_returns_130(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root, ("fonts/one.otf", "fonts/two.otf"))
            state = root / "font-state"
            output = root / "restore.json"
            binding = self._publish_and_bind(plan, state)
            apply_font_plan(plan, state=state, binding=binding)
            arguments = argparse.Namespace(
                game=plan.game_root,
                state=state,
                output=output,
                replace=False,
            )
            original_write = transaction._atomic_write_bytes  # pyright: ignore[reportPrivateUsage]

            def cancel_second_restore(target: Path, body: bytes, *, expect_missing: bool) -> None:
                if target == plan.game_root / "fonts/one.otf" and body == b"old-1":
                    raise KeyboardInterrupt
                original_write(target, body, expect_missing=expect_missing)

            with (
                patch.object(fonts, "discover_game", return_value=SimpleNamespace()),
                patch.object(fonts, "require_game_root", return_value=plan.game_root),
                patch.object(transaction, "_atomic_write_bytes", side_effect=cancel_second_restore),
                patch("att_skill_tools.core.print_error"),
                self.assertRaises(SystemExit) as raised,
            ):
                fonts.run_cli(lambda: fonts._run_restore(arguments))  # pyright: ignore[reportPrivateUsage]

            self.assertEqual(raised.exception.code, 130)
            self.assertEqual((plan.game_root / "fonts/one.otf").read_bytes(), b"new-1")
            self.assertEqual((plan.game_root / "fonts/two.otf").read_bytes(), b"new-2")
            self.assertEqual(
                json.loads((state / "status.json").read_text(encoding="utf-8")), {"status": "applied"}
            )
            self.assertFalse(output.exists())
            lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
            self.assertFalse(lock.exists())
            self.assertFalse(cleanup.exists())

    def test_apply_rejects_output_state_and_atomic_path_overlap(self) -> None:
        cases = ("same_as_output_tmp", "output_inside_state")
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                plan = self._plan(root)
                if case == "same_as_output_tmp":
                    output = root / "report.json"
                    state = root / ".report.json.tmp"
                else:
                    state = root / "font-state"
                    output = state / "report.json"
                arguments = argparse.Namespace(output=output, state=state, replace=False)

                with (
                    patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                    patch.object(fonts, "_game_root", return_value=plan.game_root),
                    patch.object(fonts, "_plan", return_value=plan),
                    self.assertRaises(ToolError) as raised,
                ):
                    fonts._run_apply(arguments)  # pyright: ignore[reportPrivateUsage]

                self.assertIn("输出路径", raised.exception.object_name)
                self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"old-1")
                self.assertFalse(state.exists())

    def test_apply_preflights_every_fixed_atomic_residual(self) -> None:
        residual_names = (
            "output_tmp",
            "state",
            "state_tmp",
            "state_previous",
            "state_tmp_cleanup",
            "state_previous_cleanup",
        )
        for residual_name in residual_names:
            with self.subTest(residual=residual_name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                plan = self._plan(root)
                output = root / "report.json"
                state = root / "font-state"
                residuals = {
                    "output_tmp": root / ".report.json.tmp",
                    "state": state,
                    "state_tmp": root / ".font-state.tmp",
                    "state_previous": root / ".font-state.previous",
                    "state_tmp_cleanup": root / ".font-state.tmp.cleanup",
                    "state_previous_cleanup": root / ".font-state.previous.cleanup",
                }
                residuals[residual_name].mkdir()
                arguments = argparse.Namespace(output=output, state=state, replace=True)

                with (
                    patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                    patch.object(fonts, "_game_root", return_value=plan.game_root),
                    patch.object(fonts, "_plan", return_value=plan),
                    self.assertRaises(ToolError),
                ):
                    fonts._run_apply(arguments)  # pyright: ignore[reportPrivateUsage]

                self.assertEqual((plan.game_root / "fonts/main.otf").read_bytes(), b"old-1")
                self.assertFalse(output.exists())

    def test_apply_rechecks_replacement_after_report_publish(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            output = root / "report.json"
            arguments = argparse.Namespace(output=output, state=state, replace=False)
            original_write_json = fonts.write_json

            def write_then_change_game(
                target: Path,
                value: JsonValue,
                *,
                replace: bool,
            ) -> None:
                original_write_json(target, value, replace=replace)
                (plan.game_root / "fonts/main.otf").write_bytes(b"late-change")

            with (
                patch.object(fonts, "_coverage_projection", return_value=self._coverage()),
                patch.object(fonts, "_game_root", return_value=plan.game_root),
                patch.object(fonts, "_plan", return_value=plan),
                patch.object(fonts, "write_json", side_effect=write_then_change_game),
                self.assertRaises(ToolError) as raised,
            ):
                fonts._run_apply(arguments)  # pyright: ignore[reportPrivateUsage]

            self.assertTrue(output.is_file())
            self.assertIn("replacement 字节不一致", raised.exception.reason)
            self.assertIn("最终游戏字节未通过验收", raised.exception.impact)
            lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
            self.assertFalse(lock.exists())
            self.assertFalse(cleanup.exists())

    def test_restore_rechecks_before_after_report_publish(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = self._plan(root)
            state = root / "font-state"
            output = root / "restore-report.json"
            binding = self._publish_and_bind(plan, state)
            apply_font_plan(plan, state=state, binding=binding)
            arguments = argparse.Namespace(
                game=plan.game_root,
                output=output,
                state=state,
                replace=False,
            )
            original_write_json = fonts.write_json

            def write_then_change_game(
                target: Path,
                value: JsonValue,
                *,
                replace: bool,
            ) -> None:
                original_write_json(target, value, replace=replace)
                (plan.game_root / "fonts/main.otf").write_bytes(b"late-change")

            with (
                patch.object(fonts, "discover_game", return_value=SimpleNamespace()),
                patch.object(fonts, "require_game_root", return_value=plan.game_root),
                patch.object(fonts, "write_json", side_effect=write_then_change_game),
                self.assertRaises(ToolError) as raised,
            ):
                fonts._run_restore(arguments)  # pyright: ignore[reportPrivateUsage]

            self.assertTrue(output.is_file())
            self.assertIn("before 字节不一致", raised.exception.reason)
            self.assertIn("最终游戏字节未通过验收", raised.exception.impact)
            lock, cleanup = transaction.font_game_lock_paths(plan.game_root)
            self.assertFalse(lock.exists())
            self.assertFalse(cleanup.exists())


if __name__ == "__main__":
    unittest.main()
