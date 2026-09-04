from __future__ import annotations

import argparse
import io
import sys
import tempfile
import unittest
from collections.abc import Generator
from contextlib import ExitStack, contextmanager, redirect_stderr
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

import inspect_nwjs_runtime as runtime
from att_skill_tools import ToolError, core


class NwjsLifecycleTests(unittest.TestCase):
    def test_stop_records_cdp_and_wait_failures_then_terminates_and_keeps_cancel(self) -> None:
        process = Mock(pid=321)
        process.poll.return_value = None
        process.wait.side_effect = [RuntimeError("private wait detail"), None]
        connection = Mock()
        connection.call.side_effect = OSError("browser close blocked")
        connection.evaluate.side_effect = KeyboardInterrupt()

        result = runtime._stop_owned_process(  # pyright: ignore[reportPrivateUsage]
            process,
            connection,
        )

        self.assertTrue(result.stopped)
        self.assertIsInstance(result.error, KeyboardInterrupt)
        process.terminate.assert_called_once_with()
        facts = "；".join(result.facts)
        self.assertIn("Browser.close", facts)
        self.assertIn("App.quit", facts)
        self.assertIn("等待 NW.js 正常退出失败", facts)
        self.assertNotIn("private wait detail", facts)

    def test_keyboard_interrupt_during_setup_cleans_work_and_remains_cancel(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            with (
                self._main_patches(root, reserve_error=KeyboardInterrupt()),
                patch("att_skill_tools.core.print_error"),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            self.assertFalse(work.exists())

    def test_setup_failure_after_work_creation_cleans_work(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            with (
                self._main_patches(root, reserve_error=OSError("port unavailable")),
                self.assertRaises(ToolError),
            ):
                runtime.main()

            self.assertFalse(work.exists())
            self.assertFalse((root / ".report.runtime.lock").exists())

    def test_existing_target_lock_blocks_setup_without_deleting_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "game").mkdir()
            work = root / ".report.runtime"
            lock = root / ".report.runtime.lock"
            lock.write_text("other task", encoding="utf-8")

            with (
                self._main_patches(
                    root,
                    reserve_error=AssertionError("port must not be reserved"),
                    mock_protect=False,
                ),
                self.assertRaises(ToolError) as raised,
            ):
                runtime.main()

            self.assertIn("已有任务锁", raised.exception.reason)
            self.assertEqual(lock.read_text(encoding="utf-8"), "other task")
            self.assertFalse(work.exists())

    def test_lock_cleanup_inspection_interrupt_exits_130_without_inventing_lock_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cleanup = root / ".report.runtime.lock.cleanup"
            real_lstat = Path.lstat
            stderr = io.StringIO()

            def interrupt_cleanup(path: Path):
                if path == cleanup:
                    raise KeyboardInterrupt
                return real_lstat(path)

            with (
                self._main_patches(root, reserve_error=AssertionError("port must not be reserved")),
                patch.object(Path, "lstat", new=interrupt_cleanup),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            self.assertNotIn("释放观察目标锁失败", stderr.getvalue())
            self.assertFalse((root / ".report.runtime.lock").exists())
            self.assertFalse((root / ".report.runtime").exists())

    def test_open_lock_interrupt_with_confirmed_absence_does_not_report_false_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stderr = io.StringIO()
            with (
                self._main_patches(root, reserve_error=AssertionError("port must not be reserved")),
                patch.object(runtime.os, "open", side_effect=KeyboardInterrupt()),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            self.assertNotIn("释放观察目标锁失败", stderr.getvalue())
            self.assertNotIn("需确认于", stderr.getvalue())
            self.assertFalse((root / ".report.runtime.lock").exists())
            self.assertFalse((root / ".report.runtime").exists())

    def test_lock_replacement_at_release_claim_is_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            lock = root / ".report.runtime.lock"
            displaced = root / "owned-lock"
            replacement = root / "replacement-lock"
            target_lock = runtime._acquire_target_lock(lock)  # pyright: ignore[reportPrivateUsage]
            replacement.write_text("foreign", encoding="utf-8")
            real_rename = runtime.os.rename
            raced = False

            def replace_before_claim(source: Path, destination: Path) -> None:
                nonlocal raced
                if Path(source) == lock and not raced:
                    raced = True
                    real_rename(lock, displaced)
                    real_rename(replacement, lock)
                real_rename(source, destination)

            with patch.object(runtime.os, "rename", side_effect=replace_before_claim):
                failure = runtime._release_target_lock(  # pyright: ignore[reportPrivateUsage]
                    lock,
                    target_lock,
                )

            self.assertIsNotNone(failure)
            self.assertEqual(lock.read_text(encoding="utf-8"), "foreign")
            self.assertTrue(displaced.is_file())
            self.assertFalse((root / ".report.runtime.lock.cleanup").exists())

    def test_lock_close_failure_reports_exact_unreleased_lock_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            lock = root / ".report.runtime.lock"
            target_lock = runtime._acquire_target_lock(lock)  # pyright: ignore[reportPrivateUsage]
            real_close = runtime.os.close
            try:
                with patch.object(runtime.os, "close", side_effect=OSError("close blocked")):
                    failure = runtime._release_target_lock(  # pyright: ignore[reportPrivateUsage]
                        lock,
                        target_lock,
                    )

                self.assertIsNotNone(failure)
                self.assertIn(str(lock), str(failure))
                self.assertTrue(lock.is_file())
                self.assertFalse((root / ".report.runtime.lock.cleanup").exists())
            finally:
                real_close(target_lock[0])
                lock.unlink(missing_ok=True)

    def test_acquire_identity_interrupt_keeps_130_and_close_failure_fact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stderr = io.StringIO()
            with (
                self._main_patches(root, reserve_error=AssertionError("port must not be reserved")),
                patch.object(runtime.os, "open", return_value=71),
                patch.object(runtime.os, "fstat", side_effect=KeyboardInterrupt()),
                patch.object(runtime.os, "close", side_effect=OSError("close blocked")),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            self.assertIn("释放观察目标锁失败", stderr.getvalue())
            self.assertIn("OSError", stderr.getvalue())

    def test_acquire_identity_error_with_close_cancel_keeps_130_and_lock_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            lock = root / ".report.runtime.lock"
            stderr = io.StringIO()
            real_close = runtime.os.close

            def close_then_interrupt(handle: int) -> None:
                real_close(handle)
                raise KeyboardInterrupt

            try:
                with (
                    self._main_patches(root, reserve_error=AssertionError("port must not be reserved")),
                    patch.object(runtime.os, "fstat", side_effect=OSError("identity unavailable")),
                    patch.object(runtime.os, "close", side_effect=close_then_interrupt),
                    redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as raised,
                ):
                    runtime.run_cli(runtime.main)

                self.assertEqual(raised.exception.code, 130)
                self.assertTrue(lock.is_file())
                self.assertIn(str(lock), stderr.getvalue())
                self.assertIn("KeyboardInterrupt", stderr.getvalue())
            finally:
                lock.unlink(missing_ok=True)

    def test_release_identity_interrupt_keeps_130_and_reports_retained_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            lock = root / ".report.runtime.lock"
            stderr = io.StringIO()
            try:
                with (
                    self._main_patches(
                        root,
                        reserve_error=AssertionError("port must not be reserved"),
                    ),
                    patch.object(runtime, "_regular_file_identity", side_effect=KeyboardInterrupt()),
                    redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as raised,
                ):
                    runtime.run_cli(runtime.main)

                self.assertEqual(raised.exception.code, 130)
                self.assertTrue(lock.is_file())
                self.assertIn(str(lock), stderr.getvalue())
                self.assertIn("释放观察目标锁失败", stderr.getvalue())
            finally:
                lock.unlink(missing_ok=True)

    def test_release_rename_probe_interrupt_keeps_130_and_both_lock_sites(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            lock = root / ".report.runtime.lock"
            claimed = root / ".report.runtime.lock.cleanup"
            stderr = io.StringIO()
            real_rename = runtime.os.rename
            real_identity = runtime._regular_file_identity  # pyright: ignore[reportPrivateUsage]
            identity_calls = 0

            def fail_lock_claim(source: Path, destination: Path) -> None:
                if Path(source) == lock and Path(destination) == claimed:
                    raise OSError("claim blocked")
                real_rename(source, destination)

            def interrupt_claimed_probe(path: Path):
                nonlocal identity_calls
                identity_calls += 1
                if identity_calls == 3:
                    raise KeyboardInterrupt
                return real_identity(path)

            try:
                with (
                    self._main_patches(
                        root,
                        reserve_error=AssertionError("port must not be reserved"),
                    ),
                    patch.object(runtime.os, "rename", side_effect=fail_lock_claim),
                    patch.object(runtime, "_regular_file_identity", side_effect=interrupt_claimed_probe),
                    redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as raised,
                ):
                    runtime.run_cli(runtime.main)

                self.assertEqual(raised.exception.code, 130)
                self.assertTrue(lock.is_file())
                self.assertIn(str(lock), stderr.getvalue())
                self.assertIn(str(claimed), stderr.getvalue())
            finally:
                lock.unlink(missing_ok=True)
                claimed.unlink(missing_ok=True)

    def test_release_unlink_probe_interrupt_keeps_130_and_claimed_lock_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            lock = root / ".report.runtime.lock"
            claimed = root / ".report.runtime.lock.cleanup"
            stderr = io.StringIO()
            real_unlink = Path.unlink
            real_identity = runtime._regular_file_identity  # pyright: ignore[reportPrivateUsage]
            identity_calls = 0

            def fail_claimed_unlink(path: Path, missing_ok: bool = False) -> None:
                if path == claimed:
                    raise OSError("unlink blocked")
                real_unlink(path, missing_ok=missing_ok)

            def interrupt_residual_probe(path: Path):
                nonlocal identity_calls
                identity_calls += 1
                if identity_calls == 3:
                    raise KeyboardInterrupt
                return real_identity(path)

            try:
                with (
                    self._main_patches(
                        root,
                        reserve_error=AssertionError("port must not be reserved"),
                    ),
                    patch.object(Path, "unlink", new=fail_claimed_unlink),
                    patch.object(runtime, "_regular_file_identity", side_effect=interrupt_residual_probe),
                    redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as raised,
                ):
                    runtime.run_cli(runtime.main)

                self.assertEqual(raised.exception.code, 130)
                self.assertFalse(lock.exists())
                self.assertTrue(claimed.is_file())
                self.assertIn(str(claimed), stderr.getvalue())
            finally:
                real_unlink(lock, missing_ok=True)
                real_unlink(claimed, missing_ok=True)

    def test_setup_error_and_release_interrupt_still_clean_work(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            lock = root / ".report.runtime.lock"
            real_identity = runtime._work_directory_identity  # pyright: ignore[reportPrivateUsage]
            identity_calls = 0

            def fail_first_identity(path: Path):
                nonlocal identity_calls
                identity_calls += 1
                if identity_calls == 1:
                    raise OSError("identity failed")
                return real_identity(path)

            def release_then_interrupt(
                _path: Path,
                target_lock: tuple[int, tuple[int, int]],
            ) -> runtime._TargetLockFailure:  # pyright: ignore[reportPrivateUsage]
                runtime.os.close(target_lock[0])
                return runtime._TargetLockFailure(  # pyright: ignore[reportPrivateUsage]
                    KeyboardInterrupt()
                )

            with (
                self._main_patches(root, reserve_error=AssertionError("port must not be reserved")),
                patch.object(runtime, "_work_directory_identity", side_effect=fail_first_identity),
                patch.object(
                    runtime,
                    "_release_target_lock",
                    side_effect=release_then_interrupt,
                ),
                self.assertRaises(ToolError) as raised,
            ):
                runtime.main()

            self.assertIn("identity failed", raised.exception.reason)
            self.assertIn("释放观察目标锁失败", raised.exception.reason)
            self.assertFalse(work.exists(), raised.exception.reason)
            self.assertTrue(lock.is_file())
            lock.unlink()

    def test_close_target_lock_handle_catches_keyboard_interrupt(self) -> None:
        with patch.object(runtime.os, "close", side_effect=KeyboardInterrupt()):
            error = runtime._close_target_lock_handle(71)  # pyright: ignore[reportPrivateUsage]

        self.assertIsInstance(error, KeyboardInterrupt)

    def test_concurrent_work_creation_is_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            marker = work / "other-process.txt"
            real_mkdir = Path.mkdir

            def race_mkdir(
                path: Path,
                mode: int = 0o777,
                parents: bool = False,
                exist_ok: bool = False,
            ) -> None:
                if path == work and not work.exists():
                    real_mkdir(path, mode=mode, parents=parents, exist_ok=exist_ok)
                    marker.write_text("owned elsewhere", encoding="utf-8")
                    raise FileExistsError(str(path))
                real_mkdir(path, mode=mode, parents=parents, exist_ok=exist_ok)

            with (
                self._main_patches(root, reserve_error=AssertionError("port must not be reserved")),
                patch.object(Path, "mkdir", new=race_mkdir),
                self.assertRaises(ToolError),
            ):
                runtime.main()

            self.assertTrue(marker.is_file())

    def test_interrupt_after_work_creation_cleans_claimed_work_and_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            lock = root / ".report.runtime.lock"
            real_mkdir = Path.mkdir

            def mkdir_then_interrupt(
                path: Path,
                mode: int = 0o777,
                parents: bool = False,
                exist_ok: bool = False,
            ) -> None:
                real_mkdir(path, mode=mode, parents=parents, exist_ok=exist_ok)
                if path == work:
                    raise KeyboardInterrupt

            with (
                self._main_patches(root, reserve_error=AssertionError("port must not be reserved")),
                patch.object(Path, "mkdir", new=mkdir_then_interrupt),
                patch("att_skill_tools.core.print_error"),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            self.assertFalse(work.exists())
            self.assertFalse(lock.exists())

    def test_created_work_without_identity_is_checked_and_reported_on_cancel(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            lock = root / ".report.runtime.lock"
            real_mkdir = Path.mkdir
            stderr = io.StringIO()

            def mkdir_then_interrupt(
                path: Path,
                mode: int = 0o777,
                parents: bool = False,
                exist_ok: bool = False,
            ) -> None:
                real_mkdir(path, mode=mode, parents=parents, exist_ok=exist_ok)
                if path == work:
                    raise KeyboardInterrupt

            with (
                self._main_patches(root, reserve_error=AssertionError("port must not be reserved")),
                patch.object(Path, "mkdir", new=mkdir_then_interrupt),
                patch.object(
                    runtime, "_work_directory_identity", side_effect=OSError("identity unavailable")
                ),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue(work.is_dir())
            self.assertFalse(lock.exists())
            self.assertIn(str(work), stderr.getvalue())
            self.assertIn("运行现场保留位置见原因", stderr.getvalue())

    def test_work_replaced_during_cleanup_is_restored_without_deletion(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            original = root / "original-runtime"
            replacement = root / "other-runtime"
            work.mkdir()
            identity = runtime._work_directory_identity(work)  # pyright: ignore[reportPrivateUsage]
            replacement.mkdir()
            marker = replacement / "other-process.txt"
            marker.write_text("owned elsewhere", encoding="utf-8")
            real_rename = runtime.os.rename
            raced = False

            def race_rename(path: Path, target: Path) -> None:
                nonlocal raced
                if Path(path) == work and not raced:
                    raced = True
                    real_rename(work, original)
                    real_rename(replacement, work)
                real_rename(path, target)

            with patch("att_skill_tools.core.os.rename", side_effect=race_rename):
                error = runtime._cleanup_work_directory(  # pyright: ignore[reportPrivateUsage]
                    work,
                    identity,
                )

            self.assertIsInstance(error, OSError)
            self.assertTrue((work / marker.name).is_file())
            self.assertTrue(original.is_dir())
            self.assertIn(str(work), str(error))
            self.assertNotIn(str(runtime._cleanup_work_path(work)), str(error))  # pyright: ignore[reportPrivateUsage]

    def test_cleanup_without_identity_distinguishes_missing_and_existing_work(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary) / ".report.runtime"
            self.assertIsNone(
                runtime._cleanup_work_directory(  # pyright: ignore[reportPrivateUsage]
                    work,
                    None,
                )
            )
            work.mkdir()

            error = runtime._cleanup_work_directory(  # pyright: ignore[reportPrivateUsage]
                work,
                None,
            )

            self.assertIsInstance(error, OSError)
            self.assertTrue(work.is_dir())

    def test_cleanup_without_identity_also_reports_cleanup_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary) / ".report.runtime"
            cleanup = Path(temporary) / ".report.runtime.cleanup"
            cleanup.mkdir()

            error = runtime._cleanup_work_directory(  # pyright: ignore[reportPrivateUsage]
                work,
                None,
            )

            self.assertIsInstance(error, runtime._WorkCleanupFailure)  # pyright: ignore[reportPrivateUsage]
            self.assertIn(str(cleanup), str(error))

    def test_cleanup_inspection_interrupt_checks_both_sites_and_keeps_cause(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary) / ".report.runtime"
            cleanup = Path(temporary) / ".report.runtime.cleanup"
            cleanup.mkdir()
            real_lstat = Path.lstat

            def interrupt_work_inspection(path: Path):
                if path == work:
                    raise KeyboardInterrupt
                return real_lstat(path)

            with patch.object(Path, "lstat", new=interrupt_work_inspection):
                error = runtime._cleanup_work_directory(  # pyright: ignore[reportPrivateUsage]
                    work,
                    None,
                )

            self.assertIsInstance(error, runtime._WorkCleanupFailure)  # pyright: ignore[reportPrivateUsage]
            assert isinstance(error, runtime._WorkCleanupFailure)  # pyright: ignore[reportPrivateUsage]
            self.assertIsInstance(error.cause, KeyboardInterrupt)
            self.assertIn(str(work), str(error))
            self.assertIn(str(cleanup), str(error))

    def test_existing_fixed_cleanup_directory_preserves_both_sites(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            cleanup = root / ".report.runtime.cleanup"
            work.mkdir()
            cleanup.mkdir()
            (work / "work.txt").write_text("work", encoding="utf-8")
            (cleanup / "cleanup.txt").write_text("cleanup", encoding="utf-8")
            identity = runtime._work_directory_identity(work)  # pyright: ignore[reportPrivateUsage]

            error = runtime._cleanup_work_directory(  # pyright: ignore[reportPrivateUsage]
                work,
                identity,
            )

            self.assertIsInstance(error, OSError)
            self.assertTrue((work / "work.txt").is_file())
            self.assertTrue((cleanup / "cleanup.txt").is_file())
            self.assertIn(str(work), str(error))
            self.assertIn(str(cleanup), str(error))

    def test_existing_fixed_cleanup_site_blocks_main_before_start(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "game").mkdir()
            work = root / ".report.runtime"
            cleanup = root / ".report.runtime.cleanup"
            cleanup.mkdir()
            marker = cleanup / "retained.txt"
            marker.write_text("retained", encoding="utf-8")

            with (
                self._main_patches(
                    root,
                    reserve_error=AssertionError("port must not be reserved"),
                    mock_protect=False,
                ),
                self.assertRaises(ToolError) as raised,
            ):
                runtime.main()

            self.assertIn("固定清理现场", raised.exception.reason)
            self.assertTrue(marker.is_file())
            self.assertFalse(work.exists())

    def test_interrupted_atomic_claim_reports_the_actual_fixed_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            claimed = root / ".report.runtime.cleanup"
            work.mkdir()
            identity = runtime._work_directory_identity(work)  # pyright: ignore[reportPrivateUsage]
            real_rename = runtime.os.rename

            def rename_then_interrupt(path: Path, target: Path) -> None:
                real_rename(path, target)
                raise KeyboardInterrupt

            with patch("att_skill_tools.core.os.rename", side_effect=rename_then_interrupt):
                error = runtime._cleanup_work_directory(  # pyright: ignore[reportPrivateUsage]
                    work,
                    identity,
                )

            self.assertIsInstance(error, runtime._WorkCleanupFailure)  # pyright: ignore[reportPrivateUsage]
            assert isinstance(error, runtime._WorkCleanupFailure)  # pyright: ignore[reportPrivateUsage]
            self.assertIsInstance(error.cause, KeyboardInterrupt)
            self.assertTrue(claimed.is_dir())
            self.assertIn(str(claimed), str(error))

    def test_keyboard_interrupt_after_process_start_stops_process_and_cleans_work(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            parser = Mock()
            parser.parse_args.return_value = self._arguments(root)
            process = Mock()
            with (
                patch.object(runtime, "_parser", return_value=parser),
                patch.object(
                    runtime,
                    "discover_game",
                    return_value=SimpleNamespace(engine="MZ", content_root=root / "game"),
                ),
                patch.object(runtime, "require_game_root", return_value=root / "game"),
                patch.object(runtime, "_runtime_entry", return_value=root / "game" / "index.html"),
                patch.object(runtime, "require_file_within", return_value=root / "game" / "Game.exe"),
                patch.object(runtime, "protect_outputs"),
                patch.object(runtime, "reserve_loopback_port", return_value=12345),
                patch.object(runtime, "build_nwjs_command", return_value=["Game.exe"]),
                patch.object(runtime.subprocess, "Popen", return_value=process),
                patch.object(
                    runtime,
                    "wait_for_owned_loopback_listener",
                    side_effect=KeyboardInterrupt(),
                ),
                patch.object(
                    runtime,
                    "_stop_owned_process",
                    return_value=runtime._ProcessStopResult(True, None, ()),  # pyright: ignore[reportPrivateUsage]
                ) as stop,
                self.assertRaises(KeyboardInterrupt),
            ):
                runtime.main()

            stop.assert_called_once_with(process, None)
            self.assertFalse(work.exists())

    def test_listener_recheck_failure_uses_common_connection_cleanup_without_masking_primary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            parser = Mock()
            parser.parse_args.return_value = self._arguments(root)
            process = Mock(pid=321)
            process.poll.return_value = None
            connection = Mock()
            connection.close.side_effect = RuntimeError("private close detail")
            stderr = io.StringIO()
            stop_result = runtime._ProcessStopResult(True, None, ())  # pyright: ignore[reportPrivateUsage]
            stop = Mock(return_value=stop_result)

            with (
                patch.multiple(
                    runtime,
                    _parser=Mock(return_value=parser),
                    discover_game=Mock(return_value=SimpleNamespace(engine="MZ", content_root=root / "game")),
                    require_game_root=Mock(return_value=root / "game"),
                    _runtime_entry=Mock(return_value=root / "game" / "index.html"),
                    require_file_within=Mock(return_value=root / "game" / "Game.exe"),
                    protect_outputs=Mock(),
                    reserve_loopback_port=Mock(return_value=12345),
                    build_nwjs_command=Mock(return_value=["Game.exe"]),
                    wait_for_owned_loopback_listener=Mock(return_value=321),
                    wait_for_page_target=Mock(
                        return_value=SimpleNamespace(
                            websocket_url="ws://runtime",
                            url="file:///game/index.html",
                        )
                    ),
                    CdpConnection=Mock(return_value=connection),
                    owned_loopback_listener_pid=Mock(
                        side_effect=runtime.CdpUnavailableError("listener changed")
                    ),
                    _stop_owned_process=stop,
                ),
                patch.object(runtime.subprocess, "Popen", return_value=process),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            message = stderr.getvalue()
            self.assertEqual(raised.exception.code, 1)
            self.assertLess(message.index("CdpUnavailableError"), message.index("RuntimeError"))
            self.assertIn("listener changed", message)
            self.assertNotIn("private close detail", message)
            stop.assert_called_once_with(process, connection)
            connection.close.assert_called_once_with()
            self.assertFalse((root / ".report.runtime").exists())

    def test_keyboard_interrupt_during_observe_returns_130_without_publishing(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            output = root / "report"
            parser = Mock()
            parser.parse_args.return_value = self._arguments(root, command="observe")
            process = Mock(pid=321)
            process.poll.return_value = None
            connection = Mock()
            observer = {"installed": True, "sequence": 0}
            with ExitStack() as stack:
                stack.enter_context(patch.object(runtime, "_parser", return_value=parser))
                stack.enter_context(
                    patch.object(
                        runtime,
                        "discover_game",
                        return_value=SimpleNamespace(engine="MZ", content_root=root / "game"),
                    )
                )
                stack.enter_context(patch.object(runtime, "require_game_root", return_value=root / "game"))
                stack.enter_context(
                    patch.object(runtime, "_runtime_entry", return_value=root / "game" / "index.html")
                )
                stack.enter_context(
                    patch.object(
                        runtime,
                        "require_file_within",
                        return_value=root / "game" / "Game.exe",
                    )
                )
                stack.enter_context(patch.object(runtime, "reserve_loopback_port", return_value=12345))
                stack.enter_context(patch.object(runtime, "build_nwjs_command", return_value=["Game.exe"]))
                stack.enter_context(
                    patch.object(runtime, "wait_for_owned_loopback_listener", return_value=321)
                )
                stack.enter_context(patch.object(runtime, "owned_loopback_listener_pid", return_value=321))
                stack.enter_context(patch.object(runtime, "_wait_for_observer", return_value=observer))
                stack.enter_context(
                    patch.object(
                        runtime,
                        "wait_for_runtime_start",
                        return_value=runtime.StartupObservation("ready", "Scene_Title", 0.0, ()),
                    )
                )
                stack.enter_context(patch.object(runtime, "_take_observation", return_value=([], observer)))
                stack.enter_context(patch.object(runtime, "protect_outputs"))
                stack.enter_context(patch.object(runtime.subprocess, "Popen", return_value=process))
                stack.enter_context(
                    patch.object(
                        runtime,
                        "wait_for_page_target",
                        return_value=SimpleNamespace(
                            websocket_url="ws://runtime", url="file:///game/index.html"
                        ),
                    )
                )
                stack.enter_context(patch.object(runtime, "CdpConnection", return_value=connection))
                stack.enter_context(patch.object(runtime.time, "sleep", side_effect=KeyboardInterrupt()))
                stop = stack.enter_context(
                    patch.object(
                        runtime,
                        "_stop_owned_process",
                        return_value=runtime._ProcessStopResult(True, None, ()),  # pyright: ignore[reportPrivateUsage]
                    )
                )
                publish = stack.enter_context(patch.object(runtime, "atomic_write_directory"))
                stack.enter_context(patch("att_skill_tools.core.print_error"))
                raised = stack.enter_context(self.assertRaises(SystemExit))
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            stop.assert_called_once_with(process, connection)
            publish.assert_not_called()
            self.assertFalse(work.exists())
            self.assertFalse(output.exists())

    def test_cancel_with_cleanup_problems_reports_them_and_keeps_130(self) -> None:
        stderr = io.StringIO()

        def command() -> int:
            runtime._raise_observation_failure(  # pyright: ignore[reportPrivateUsage]
                KeyboardInterrupt(),
                game_root=Path("D:/game"),
                work=Path("D:/review/.report.runtime"),
                stop_problem="本工具启动的 PID 仍在运行",
                cleanup_error=OSError("现场保留于 D:/review/.report.runtime"),
            )

        with (
            redirect_stderr(stderr),
            self.assertRaises(SystemExit) as raised,
        ):
            runtime.run_cli(command)

        self.assertEqual(raised.exception.code, 130)
        self.assertIn("PID 仍在运行", stderr.getvalue())
        self.assertIn("现场保留于", stderr.getvalue())

    def test_cleanup_interrupt_after_publication_reports_published_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "report"
            parser = Mock()
            parser.parse_args.return_value = self._arguments(root)
            process = Mock(pid=321)
            process.poll.return_value = 1
            connection = Mock()
            observer = {"installed": True, "sequence": 0}
            stderr = io.StringIO()

            def publish(path: Path, _files: object, *, replace: bool) -> None:
                self.assertFalse(replace)
                path.mkdir()
                (path / "report.json").write_text("published", encoding="utf-8")

            with (
                patch.multiple(
                    runtime,
                    _parser=Mock(return_value=parser),
                    discover_game=Mock(return_value=SimpleNamespace(engine="MZ", content_root=root / "game")),
                    require_game_root=Mock(return_value=root / "game"),
                    _runtime_entry=Mock(return_value=root / "game" / "index.html"),
                    require_file_within=Mock(return_value=root / "game" / "Game.exe"),
                    protect_outputs=Mock(),
                    reserve_loopback_port=Mock(return_value=12345),
                    build_nwjs_command=Mock(return_value=["Game.exe"]),
                    wait_for_owned_loopback_listener=Mock(return_value=321),
                    wait_for_page_target=Mock(
                        return_value=SimpleNamespace(
                            websocket_url="ws://runtime",
                            url="file:///game/index.html",
                        )
                    ),
                    CdpConnection=Mock(return_value=connection),
                    owned_loopback_listener_pid=Mock(return_value=321),
                    _wait_for_observer=Mock(return_value=observer),
                    wait_for_runtime_start=Mock(
                        return_value=runtime.StartupObservation("process_exited", "", 0.0, ())
                    ),
                    _stop_owned_process=Mock(
                        return_value=runtime._ProcessStopResult(True, None, ())  # pyright: ignore[reportPrivateUsage]
                    ),
                    atomic_write_directory=Mock(side_effect=publish),
                    _cleanup_work_directory=Mock(return_value=KeyboardInterrupt()),
                ),
                patch.object(runtime.subprocess, "Popen", return_value=process),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue((output / "report.json").is_file())
            self.assertIn("观察报告已经发布", stderr.getvalue())
            self.assertIn("完整报告位于", stderr.getvalue())

    def test_final_presentation_interrupt_after_publication_cleans_work_and_exits_130(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "report"
            work = root / ".report.runtime"
            output.mkdir()
            (output / "report.json").write_text("published", encoding="utf-8")
            work.mkdir()
            work_identity = runtime._work_directory_identity(work)  # pyright: ignore[reportPrivateUsage]
            stderr = io.StringIO()

            def complete() -> int:
                return runtime._complete_published_observation(  # pyright: ignore[reportPrivateUsage]
                    output,
                    work,
                    work_identity,
                )

            with (
                patch.object(runtime, "_published_completion", side_effect=KeyboardInterrupt()),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(complete)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue((output / "report.json").is_file())
            self.assertFalse(work.exists())
            self.assertIn("观察报告已经发布", stderr.getvalue())
            self.assertIn("完整报告位于", stderr.getvalue())

    def test_cleanup_failure_after_publication_uses_published_error_for_non_cancel_failure(self) -> None:
        output = Path("D:/review/report")
        work = Path("D:/review/.report.runtime")
        with (
            patch.object(runtime, "_cleanup_work_directory", return_value=PermissionError("blocked")),
            self.assertRaises(runtime.OutputPublishedError) as raised,
        ):
            runtime._complete_published_observation(  # pyright: ignore[reportPrivateUsage]
                output,
                work,
                (1, 2),
            )

        self.assertIsInstance(raised.exception.cause, PermissionError)
        self.assertIn("完整报告位于", raised.exception.impact)

    def test_interrupt_after_report_rename_keeps_published_report_and_returns_130(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "report"
            work = root / ".report.runtime"
            parser = Mock()
            parser.parse_args.return_value = self._arguments(root)
            process = Mock(pid=321)
            process.poll.return_value = 1
            connection = Mock()
            observer = {"installed": True, "sequence": 0}
            stderr = io.StringIO()
            real_rename = runtime.os.rename

            def interrupt_after_report_rename(source: Path, destination: Path) -> None:
                real_rename(source, destination)
                if Path(source) == output.with_name(".report.tmp") and Path(destination) == output:
                    raise KeyboardInterrupt

            with (
                patch.multiple(
                    runtime,
                    _parser=Mock(return_value=parser),
                    discover_game=Mock(return_value=SimpleNamespace(engine="MZ", content_root=root / "game")),
                    require_game_root=Mock(return_value=root / "game"),
                    _runtime_entry=Mock(return_value=root / "game" / "index.html"),
                    require_file_within=Mock(return_value=root / "game" / "Game.exe"),
                    protect_outputs=Mock(),
                    reserve_loopback_port=Mock(return_value=12345),
                    build_nwjs_command=Mock(return_value=["Game.exe"]),
                    wait_for_owned_loopback_listener=Mock(return_value=321),
                    wait_for_page_target=Mock(
                        return_value=SimpleNamespace(
                            websocket_url="ws://runtime",
                            url="file:///game/index.html",
                        )
                    ),
                    CdpConnection=Mock(return_value=connection),
                    owned_loopback_listener_pid=Mock(return_value=321),
                    _wait_for_observer=Mock(return_value=observer),
                    wait_for_runtime_start=Mock(
                        return_value=runtime.StartupObservation("process_exited", "", 0.0, ())
                    ),
                    _stop_owned_process=Mock(
                        return_value=runtime._ProcessStopResult(True, None, ())  # pyright: ignore[reportPrivateUsage]
                    ),
                ),
                patch.object(runtime.subprocess, "Popen", return_value=process),
                patch("att_skill_tools.core.os.rename", side_effect=interrupt_after_report_rename),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            self.assertEqual(raised.exception.code, 130)
            self.assertTrue((output / "report.json").is_file())
            self.assertFalse(work.exists())
            self.assertFalse((root / ".report.runtime.lock").exists())
            self.assertIn("完整观察报告已经发布", stderr.getvalue())

    def test_stop_and_cleanup_errors_do_not_mask_observation_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            parser = Mock()
            parser.parse_args.return_value = self._arguments(root)
            stderr = io.StringIO()

            with (
                patch.object(runtime, "_parser", return_value=parser),
                patch.object(
                    runtime,
                    "discover_game",
                    return_value=SimpleNamespace(engine="MZ", content_root=root / "game"),
                ),
                patch.object(runtime, "require_game_root", return_value=root / "game"),
                patch.object(runtime, "_runtime_entry", return_value=root / "game" / "index.html"),
                patch.object(runtime, "require_file_within", return_value=root / "game" / "Game.exe"),
                patch.object(runtime, "protect_outputs"),
                patch.object(runtime, "reserve_loopback_port", return_value=12345),
                patch.object(runtime, "build_nwjs_command", return_value=["Game.exe"]),
                patch.object(runtime.subprocess, "Popen", return_value=Mock()),
                patch.object(
                    runtime,
                    "wait_for_owned_loopback_listener",
                    side_effect=ValueError("primary observation error"),
                ),
                patch.object(runtime, "_stop_owned_process", side_effect=RuntimeError("stop failed")),
                patch(
                    "att_skill_tools.core.shutil.rmtree",
                    side_effect=PermissionError("cleanup blocked"),
                ),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            message = stderr.getvalue()
            self.assertEqual(raised.exception.code, 1)
            self.assertLess(message.index("ValueError"), message.index("RuntimeError"))
            self.assertNotIn("primary observation error", message)
            self.assertNotIn("stop failed", message)
            self.assertIn("固定运行现场无法清理", message)
            self.assertIn(str(work), message)
            self.assertTrue(work.exists())

    def test_rmtree_error_after_removal_reports_cleaned_runtime_site(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            cleanup = root / ".report.runtime.cleanup"
            stderr = io.StringIO()
            real_rmtree = core.shutil.rmtree

            def remove_then_fail(path: Path) -> None:
                real_rmtree(path)
                raise PermissionError("rmtree returned after removal")

            with (
                self._main_patches(root, reserve_error=ValueError("primary setup error")),
                patch.object(core.shutil, "rmtree", side_effect=remove_then_fail),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            message = stderr.getvalue()
            self.assertEqual(raised.exception.code, 1)
            self.assertFalse(work.exists())
            self.assertFalse(cleanup.exists())
            self.assertIn("后验确认固定运行现场已经清理", message)
            self.assertIn("运行现场已经清理", message)
            self.assertNotIn("运行现场保留位置见原因", message)

    def test_cleanup_interrupt_after_removal_keeps_cancel_and_cleaned_fact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / ".report.runtime"
            cleanup = root / ".report.runtime.cleanup"
            stderr = io.StringIO()
            real_rmtree = core.shutil.rmtree

            def remove_then_interrupt(path: Path) -> None:
                real_rmtree(path)
                raise KeyboardInterrupt

            with (
                self._main_patches(root, reserve_error=ValueError("primary setup error")),
                patch.object(core.shutil, "rmtree", side_effect=remove_then_interrupt),
                redirect_stderr(stderr),
                self.assertRaises(SystemExit) as raised,
            ):
                runtime.run_cli(runtime.main)

            message = stderr.getvalue()
            self.assertEqual(raised.exception.code, 130)
            self.assertFalse(work.exists())
            self.assertFalse(cleanup.exists())
            self.assertIn("后验确认固定运行现场已经清理", message)
            self.assertIn("运行现场已经清理", message)

    @contextmanager
    def _main_patches(
        self,
        root: Path,
        *,
        reserve_error: BaseException,
        mock_protect: bool = True,
    ) -> Generator[None]:
        parser = Mock()
        parser.parse_args.return_value = self._arguments(root)
        with ExitStack() as stack:
            stack.enter_context(
                patch.multiple(
                    runtime,
                    _parser=Mock(return_value=parser),
                    discover_game=Mock(return_value=SimpleNamespace(engine="MZ", content_root=root / "game")),
                    require_game_root=Mock(return_value=root / "game"),
                    _runtime_entry=Mock(return_value=root / "game" / "index.html"),
                    require_file_within=Mock(return_value=root / "game" / "Game.exe"),
                    reserve_loopback_port=Mock(side_effect=reserve_error),
                )
            )
            if mock_protect:
                stack.enter_context(patch.object(runtime, "protect_outputs"))
            yield

    @staticmethod
    def _arguments(root: Path, *, command: str = "smoke") -> argparse.Namespace:
        return argparse.Namespace(
            command=command,
            game=root / "game",
            output=root / "report",
            confirm_isolated_copy=True,
            startup_timeout=1.0,
            settle_ms=1,
            duration=None,
        )


if __name__ == "__main__":
    unittest.main()
