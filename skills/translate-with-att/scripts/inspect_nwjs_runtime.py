#!/usr/bin/env python3
"""在隔离游戏副本中通过 CDP 记录 RPG Maker NW.js 实际绘制文本。"""

from __future__ import annotations

import argparse
import base64
import json
import math
import os
import stat
import subprocess
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, NoReturn, TextIO, cast

# Skill 目录是发行资源，入口进程不得把解释器缓存写回包内。
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    DirectoryPublishedError,
    ToolArgumentParser,
    ToolCancelledError,
    ToolError,
    atomic_write_directory,
    fail,
    protect_outputs,
    remove_owned_directory,
    require_file_within,
    run_cli,
)
from att_toolbox.nwjs import (
    OBSERVER_SCRIPT,
    CdpConnection,
    CdpEvaluationError,
    CdpProtocolError,
    CdpUnavailableError,
    build_nwjs_command,
    owned_loopback_listener_pid,
    process_tree_pids,
    reserve_loopback_port,
    scenario_expression,
    wait_for_owned_loopback_listener,
    wait_for_page_target,
)
from att_toolbox.png import decode_png_size
from att_toolbox.rpg import discover_game, require_game_root

_SCENARIOS = ("title", "new_game", "dialogue", "menu", "quest_log", "options", "save")
_DRAW_KINDS = frozenset({"Bitmap.drawText", "Window_Base.drawText", "Window_Base.drawTextEx"})
_OBSERVER_HOOKS = frozenset(
    {
        "bitmapDrawText",
        "windowDrawText",
        "windowDrawTextEx",
        "addCommand",
        "loadFont",
        "fontManagerLoad",
        "graphicsPrintError",
        "graphicsPrintLoadingError",
    }
)
_MIN_REVIEW_SCREENSHOT_DIMENSION = 64


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(
        description="启动隔离的 RPG Maker MV/MZ 副本，通过本地 CDP 记录实际绘制英文和像素越界。"
    )
    commands = parser.add_subparsers(dest="command", required=True)

    def add_common(command: argparse.ArgumentParser) -> None:
        command.add_argument("--game", type=Path, required=True, help="可丢弃的完整游戏副本")
        command.add_argument("--output", type=Path, required=True, help="新建的观察报告目录")
        command.add_argument(
            "--confirm-isolated-copy",
            action="store_true",
            help="确认 --game 是可丢弃副本，不是源游戏或唯一成品",
        )
        command.add_argument(
            "--startup-timeout",
            type=float,
            default=75.0,
            help="等待 CDP 和游戏离开启动场景的秒数；默认覆盖 MV 的 60 秒字体失败窗口",
        )

    smoke = commands.add_parser("smoke", help="不注入键盘地检查预定义标题、菜单和游戏场景")
    add_common(smoke)
    smoke.add_argument("--settle-ms", type=int, default=900, help="每个场景切换后的观察毫秒数")
    observe = commands.add_parser("observe", help="只记录用户或 Agent 的正常鼠标游玩，不自动切换场景")
    add_common(observe)
    observe.add_argument("--duration", type=float, help="可选持续秒数；省略时直到游戏关闭或 Ctrl+C")
    return parser


def _owned_process_exited(process: subprocess.Popen[bytes]) -> bool:
    return process.poll() is not None


def _runtime_entry(content_root: Path) -> Path:
    package_path = require_file_within(content_root / "package.json", content_root, "NW.js package.json")
    try:
        package = cast(object, json.loads(package_path.read_text(encoding="utf-8-sig")))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(
            str(package_path),
            f"NW.js package.json 无法读取（{type(error).__name__}）",
            "恢复目标游戏的完整入口配置",
        )
    if not isinstance(package, Mapping):
        fail(str(package_path), "NW.js package.json 根值不是 object", "恢复目标游戏的完整入口配置")
    main = cast(Mapping[object, object], package).get("main")
    if not isinstance(main, str) or not main.strip():
        fail(str(package_path), "NW.js package.json 缺少有效 main", "填写该内容根实际使用的 HTML 入口")
    normalized = main.replace("\\", "/").split("?", 1)[0].split("#", 1)[0]
    relative = PurePosixPath(normalized)
    if relative.is_absolute() or ".." in relative.parts or ":" in normalized:
        fail(
            str(package_path), "NW.js main 不是内容根内的自然相对路径", "恢复目标游戏实际使用的本地 HTML 入口"
        )
    entry = require_file_within(content_root.joinpath(*relative.parts), content_root, "NW.js HTML 入口")
    if entry.suffix.casefold() not in {".htm", ".html"}:
        fail(str(entry), "NW.js main 不是 HTML 入口", "恢复 RPG Maker 实际使用的 HTML 入口")
    return entry


@dataclass(frozen=True, slots=True)
class _ProcessStopResult:
    stopped: bool
    error: BaseException | None
    facts: tuple[str, ...]


def _stop_owned_process(
    process: subprocess.Popen[bytes], connection: CdpConnection | None
) -> _ProcessStopResult:
    """只关闭本工具启动并能由 Toolhelp32 证明的进程树。"""

    errors: list[tuple[str, BaseException]] = []
    cancellation: KeyboardInterrupt | None = None

    def remember(label: str, error: BaseException) -> None:
        nonlocal cancellation
        errors.append((label, error))
        if isinstance(error, KeyboardInterrupt) and cancellation is None:
            cancellation = error

    def fact(label: str, error: BaseException) -> str:
        known = (CdpProtocolError, CdpUnavailableError, OSError, subprocess.SubprocessError)
        detail = str(error).strip() if isinstance(error, known) else ""
        return (
            f"{label}（{type(error).__name__}）：{detail}" if detail else f"{label}（{type(error).__name__}）"
        )

    def result(*, stopped: bool, final_error: BaseException | None = None) -> _ProcessStopResult:
        primary = cancellation or final_error
        return _ProcessStopResult(
            stopped=stopped,
            error=primary,
            facts=tuple(
                fact(label, error)
                for label, error in errors
                if cancellation is not None or error is not final_error
            ),
        )

    def descendants_stopped() -> bool:
        if os.name != "nt":
            return True
        return not (process_tree_pids(process.pid, require_root=False) - {process.pid})

    try:
        already_exited = process.poll() is not None
    except BaseException as error:  # noqa: BLE001 - 停止流程继续收集可验证的进程事实。
        remember("读取本工具启动的 NW.js 进程状态失败", error)
        already_exited = False
    if already_exited:
        try:
            descendants_are_stopped = descendants_stopped()
        except BaseException as error:  # noqa: BLE001 - 后代核对失败是最终停止事实。
            remember("核对本工具启动的 NW.js 后代进程失败", error)
            return result(stopped=False, final_error=error)
        if descendants_are_stopped:
            return result(stopped=True)
        return _ProcessStopResult(
            stopped=False,
            error=cancellation,
            facts=(
                *(fact(label, error) for label, error in errors),
                "本工具启动的 NW.js 后代进程仍在运行，工具未终止这些 PID",
            ),
        )
    if connection is not None:
        try:
            connection.call("Browser.close")
        except BaseException as error:  # noqa: BLE001 - 先保存 CDP 事实，再继续 OS 级停止。
            remember("通过 CDP Browser.close 关闭 NW.js 失败", error)
            try:
                connection.evaluate("window.nw && nw.App ? (nw.App.quit(), true) : false")
            except BaseException as evaluate_error:  # noqa: BLE001 - 回退失败不阻断 OS 级停止。
                remember("通过 NW.js App.quit 关闭进程失败", evaluate_error)
    try:
        process.wait(timeout=4.0)
    except subprocess.TimeoutExpired:
        pass
    except BaseException as error:  # noqa: BLE001 - 等待异常后仍继续 OS 级 terminate。
        remember("等待 NW.js 正常退出失败", error)
    else:
        try:
            descendants_are_stopped = descendants_stopped()
        except BaseException as error:  # noqa: BLE001 - 后代核对失败是最终停止事实。
            remember("核对本工具启动的 NW.js 后代进程失败", error)
            return result(stopped=False, final_error=error)
        if descendants_are_stopped:
            return result(stopped=True)
        errors.append(("本工具启动的 NW.js 后代进程仍在运行", OSError("工具未终止这些 PID")))

    final_error: BaseException | None = None
    try:
        process.terminate()
    except BaseException as error:  # noqa: BLE001 - terminate 返回后仍核对最终进程状态。
        remember("终止本工具启动的 NW.js PID 失败", error)
        final_error = error
    try:
        process.wait(timeout=4.0)
    except BaseException as error:  # noqa: BLE001 - 最终等待结果决定停止是否完成。
        remember("等待已终止的 NW.js PID 退出失败", error)
        final_error = error
    else:
        try:
            descendants_are_stopped = descendants_stopped()
        except BaseException as error:  # noqa: BLE001 - 后代核对失败是最终停止事实。
            remember("核对本工具启动的 NW.js 后代进程失败", error)
            return result(stopped=False, final_error=error)
        if descendants_are_stopped:
            return result(stopped=True)
        final_error = OSError("本工具启动的 NW.js 后代进程仍在运行")
        errors.append(("核对停止结果失败", final_error))
    return result(stopped=False, final_error=final_error)


def _mapping(value: object) -> dict[str, object]:
    if not isinstance(value, Mapping):
        return {"supported": False, "reason": "runtime_result_not_object"}
    return {str(key): item for key, item in cast(Mapping[object, object], value).items()}


def scenario_action(connection: CdpConnection, name: str) -> dict[str, object]:
    """把页面脚本异常限制在当前场景；连接或协议错误仍由调用方终止整次观察。"""

    try:
        return _mapping(connection.evaluate(scenario_expression(name)))
    except CdpEvaluationError:
        return {"supported": False, "reason": "scenario_script_exception"}


def _event_list(value: object) -> list[dict[str, object]]:
    if not isinstance(value, list):
        raise CdpProtocolError("观察器事件结果不是 array")
    result: list[dict[str, object]] = []
    for item in cast(list[object], value):
        if not isinstance(item, Mapping):
            raise CdpProtocolError("观察器事件项不是 object")
        result.append({str(key): member for key, member in cast(Mapping[object, object], item).items()})
    return result


def _capture_screenshot(connection: CdpConnection) -> bytes:
    result = connection.call("Page.captureScreenshot", {"format": "png", "fromSurface": True})
    data = result.get("data")
    if not isinstance(data, str):
        raise CdpProtocolError("Page.captureScreenshot 缺少 PNG 数据")
    try:
        screenshot = base64.b64decode(data, validate=True)
    except ValueError as error:
        raise CdpProtocolError("Page.captureScreenshot 返回无效 base64") from error
    try:
        decode_png_size(screenshot)
    except ValueError as error:
        raise CdpProtocolError("Page.captureScreenshot 返回无法解码的非空 PNG") from error
    return screenshot


def _observer_ready(snapshot: Mapping[str, object]) -> bool:
    requirements = snapshot.get("hookRequirements")
    sequence = snapshot.get("sequence")
    typed_requirements: Mapping[object, object] = (
        cast(Mapping[object, object], requirements)
        if isinstance(requirements, Mapping)
        else cast(Mapping[object, object], {})
    )
    return bool(
        snapshot.get("installed") is True
        and snapshot.get("requiredHooksInstalled") is True
        and snapshot.get("pageLoadFinished") is True
        and snapshot.get("pollingObserved") is True
        and snapshot.get("installationFinished") is True
        and len(typed_requirements) == len(_OBSERVER_HOOKS)
        and all(isinstance(key, str) for key in typed_requirements)
        and {cast(str, key) for key in typed_requirements} == set(_OBSERVER_HOOKS)
        and all(value is True for value in typed_requirements.values())
        and isinstance(sequence, int)
        and not isinstance(sequence, bool)
        and sequence >= 0
    )


def _snapshot_sequence(snapshot: Mapping[str, object]) -> int:
    sequence = snapshot.get("sequence")
    if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence < 0:
        raise CdpProtocolError("观察器快照缺少有效事件序列边界")
    return sequence


def _event_sequence(event: Mapping[str, object]) -> int:
    sequence = event.get("sequence")
    if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence <= 0:
        raise CdpProtocolError("观察器事件缺少有效序列")
    return sequence


def _wait_for_observer(connection: CdpConnection, timeout: float) -> dict[str, object]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = connection.evaluate(
            "window.__ATT_NW_OBSERVER__ ? __ATT_NW_OBSERVER__.snapshot() : ({installed:false})"
        )
        result = _mapping(value)
        if (
            result.get("installed") is True
            and result.get("pageLoadFinished") is True
            and result.get("pollingObserved") is True
        ):
            return result
        time.sleep(0.05)
    raise CdpUnavailableError("页面观察器没有建立")


def _take_observation(connection: CdpConnection) -> tuple[list[dict[str, object]], dict[str, object]]:
    value = connection.evaluate(
        "(() => { const events = __ATT_NW_OBSERVER__.take(); "
        "return {events:events,snapshot:__ATT_NW_OBSERVER__.snapshot()}; })()"
    )
    result = _mapping(value)
    return _event_list(result.get("events")), _mapping(result.get("snapshot"))


def _take_runtime_errors(connection: CdpConnection) -> list[dict[str, object]]:
    return _event_list(connection.evaluate("__ATT_NW_OBSERVER__.takeErrors()"))


@dataclass(frozen=True, slots=True)
class StartupObservation:
    status: str
    scene: str
    wait_seconds: float
    runtime_errors: tuple[dict[str, object], ...]


def wait_for_runtime_start(
    connection: CdpConnection,
    *,
    timeout: float,
    process_exited: Callable[[], bool],
    poll_seconds: float = 0.05,
) -> StartupObservation:
    """等待游戏离开 Scene_Boot，或取得阻止正常启动的直接证据。"""

    started = time.monotonic()
    deadline = started + timeout
    runtime_errors: list[dict[str, object]] = []
    scene = ""
    while True:
        elapsed = time.monotonic() - started
        if process_exited():
            return StartupObservation("process_exited", scene, elapsed, tuple(runtime_errors))
        runtime_errors.extend(_take_runtime_errors(connection))
        state = _mapping(connection.evaluate("({scene:__ATT_NW_OBSERVER__.scene()})"))
        value = state.get("scene")
        scene = value if isinstance(value, str) else ""
        elapsed = time.monotonic() - started
        if runtime_errors:
            return StartupObservation("runtime_error", scene, elapsed, tuple(runtime_errors))
        if scene and scene != "Scene_Boot":
            return StartupObservation("ready", scene, elapsed, ())
        if elapsed >= timeout:
            return StartupObservation("timed_out", scene, elapsed, ())
        time.sleep(min(poll_seconds, max(0.0, deadline - time.monotonic())))


@dataclass(slots=True)
class _ObservationStats:
    event_count: int = 0
    draw_count: int = 0
    english_count: int = 0
    overflow_count: int = 0
    requested_font_not_loaded_count: int = 0
    measurement_unverified_count: int = 0
    glyph_fallback_unverified: bool = False
    runtime_error_count: int = 0
    last_event_sequence: int = 0


def _write_event(handle: TextIO, event: Mapping[str, object]) -> None:
    handle.write(json.dumps(dict(event), ensure_ascii=False, sort_keys=True) + "\n")


def _record_events(
    work: Path,
    events: list[dict[str, object]],
    stats: _ObservationStats,
    *,
    phase: str,
    scenario: str | None = None,
) -> list[dict[str, object]]:
    if not events:
        return []
    event_path = work / "events.jsonl"
    draws = work / "draws.jsonl"
    english_path = work / "english-candidates.jsonl"
    overflow_path = work / "pixel-overflows.jsonl"
    measurement_path = work / "layout-measurement-unverified.jsonl"
    font_path = work / "font-load-review.jsonl"
    recorded_draws: list[dict[str, object]] = []
    with (
        event_path.open("a", encoding="utf-8", newline="\n") as event_handle,
        draws.open("a", encoding="utf-8", newline="\n") as draw_handle,
        english_path.open("a", encoding="utf-8", newline="\n") as english_handle,
        overflow_path.open("a", encoding="utf-8", newline="\n") as overflow_handle,
        measurement_path.open("a", encoding="utf-8", newline="\n") as measurement_handle,
        font_path.open("a", encoding="utf-8", newline="\n") as font_handle,
    ):
        for raw_event in events:
            sequence = raw_event.get("sequence")
            if (
                not isinstance(sequence, int)
                or isinstance(sequence, bool)
                or sequence <= stats.last_event_sequence
                or not isinstance(raw_event.get("timestampMs"), (int, float))
                or isinstance(raw_event.get("timestampMs"), bool)
                or not isinstance(raw_event.get("kind"), str)
                or not isinstance(raw_event.get("text"), str)
                or not isinstance(raw_event.get("scene"), str)
                or not isinstance(raw_event.get("context"), str)
                or not isinstance(raw_event.get("geometry"), Mapping)
                or not isinstance(raw_event.get("font"), Mapping)
            ):
                raise CdpProtocolError("观察器事件缺少递增序列或完整绘制语义")
            stats.last_event_sequence = sequence
            event = dict(raw_event)
            event["observation_scope"] = {"phase": phase, "scenario": scenario}
            _write_event(event_handle, event)
            stats.event_count += 1
            is_draw = event["kind"] in _DRAW_KINDS
            if is_draw:
                _write_event(draw_handle, event)
                recorded_draws.append(event)
                stats.draw_count += 1
            text = event.get("text")
            if is_draw and isinstance(text, str) and text:
                stats.glyph_fallback_unverified = True
                if any("A" <= character <= "Z" or "a" <= character <= "z" for character in text):
                    stats.english_count += 1
                    _write_event(english_handle, event)
            geometry = event.get("geometry")
            if is_draw and isinstance(geometry, Mapping):
                typed_geometry = cast(Mapping[object, object], geometry)
                if any(
                    typed_geometry.get(field) is True
                    for field in ("clippingOverflow", "overflowLeft", "overflowRight", "overflowBottom")
                ):
                    stats.overflow_count += 1
                    _write_event(overflow_handle, event)
                measurement_status = typed_geometry.get("measurementStatus")
                if isinstance(measurement_status, str) and measurement_status.startswith("unverified_"):
                    stats.measurement_unverified_count += 1
                    _write_event(measurement_handle, event)
            font = event.get("font")
            if (
                isinstance(font, Mapping)
                and cast(Mapping[object, object], font).get("requestedFontLoaded") is False
            ):
                stats.requested_font_not_loaded_count += 1
                _write_event(font_handle, event)
        for handle in (
            event_handle,
            draw_handle,
            english_handle,
            overflow_handle,
            measurement_handle,
            font_handle,
        ):
            handle.flush()
    return recorded_draws


def _record_runtime_errors(
    work: Path,
    errors: list[dict[str, object]] | tuple[dict[str, object], ...],
    stats: _ObservationStats,
    *,
    phase: str,
    scenario: str | None = None,
) -> None:
    if not errors:
        return
    with (work / "runtime-errors.jsonl").open("a", encoding="utf-8", newline="\n") as handle:
        for raw_error in errors:
            error = dict(raw_error)
            error["observation_scope"] = {"phase": phase, "scenario": scenario}
            _write_event(handle, error)
            stats.runtime_error_count += 1
        handle.flush()


def scenario_status(
    name: str,
    action: Mapping[str, object],
    events: Sequence[Mapping[str, object]],
    *,
    observer_start: Mapping[str, object] | None = None,
    observer_end: Mapping[str, object] | None = None,
    screenshot_size: tuple[int, int] | None = None,
) -> tuple[str, str]:
    if action.get("supported") is not True:
        return "unverified", "运行时不支持或无法唯一定位该场景"
    if observer_start is not None and not _observer_ready(observer_start):
        return "unverified", "场景开始时观察器 hooks 或安装轮询不完整"
    if observer_end is not None and not _observer_ready(observer_end):
        return "unverified", "场景结束时观察器 hooks 或安装轮询不完整"
    if screenshot_size is not None and (
        screenshot_size[0] < _MIN_REVIEW_SCREENSHOT_DIMENSION
        or screenshot_size[1] < _MIN_REVIEW_SCREENSHOT_DIMENSION
    ):
        return "unverified", "场景截图尺寸不足以审核界面文本"
    if not events:
        return "unverified", "当前场景没有观察到实际文本绘制"
    if name == "dialogue":
        return (
            ("verified", "观察到 Window_Message 的真实 drawTextEx")
            if any(
                event.get("kind") == "Window_Base.drawTextEx"
                and isinstance(event.get("text"), str)
                and bool(cast(str, event["text"]).strip())
                and isinstance(event.get("context"), str)
                and "Window_Message" in cast(str, event["context"])
                for event in events
            )
            else ("unverified", "没有观察到真实消息窗口文本绘制")
        )
    expected = {
        "title": "Scene_Title",
        "new_game": "Scene_Map",
        "menu": "Menu",
        "quest_log": cast(str, action.get("sceneClass", "Quest")),
        "options": "Options",
        "save": "Save",
    }.get(name, name)
    if any(
        isinstance(event.get("text"), str)
        and bool(cast(str, event["text"]).strip())
        and isinstance(value, str)
        and expected.casefold() in value.casefold()
        for event in events
        for value in (event.get("scene"), event.get("context"))
    ):
        return "verified", f"观察到 {expected} 的实际绘制"
    return "unverified", f"没有观察到 {expected} 的实际绘制"


def _font_review(stats: _ObservationStats) -> dict[str, object]:
    return {
        "requested_font_not_loaded_count": stats.requested_font_not_loaded_count,
        "requested_font_not_loaded_file": "font-load-review.jsonl",
        "glyph_fallback_status": "unverified" if stats.glyph_fallback_unverified else "not_observed",
        "glyph_fallback_unverified": stats.glyph_fallback_unverified,
        "interpretation": "document.fonts.check 只能确认请求 family 的加载状态，不能证明每个实际 glyph 未回退。",
    }


def _json_text(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def _published_completion(output: Path) -> int:
    print(f"NW.js 观察完成：{output.resolve(strict=False)}", flush=True)
    return 0


_WorkDirectoryIdentity = tuple[int, int]
_TargetLock = tuple[int, _WorkDirectoryIdentity]


@dataclass(slots=True)
class _TargetLockFailure(Exception):
    primary: BaseException
    secondary: BaseException | None = None

    def __str__(self) -> str:
        detail = f"；附加事实 {type(self.secondary).__name__}：{self.secondary}" if self.secondary else ""
        return f"观察目标锁操作发生 {type(self.primary).__name__}{detail}"


class _WorkCleanupFailure(OSError):
    def __init__(self, cause: BaseException, retained_sites: tuple[str, ...]) -> None:
        self.cause = cause
        self.retained_sites = retained_sites
        state = (
            f"固定运行现场保留于或需确认于 {'；'.join(retained_sites)}"
            if retained_sites
            else "后验确认固定运行现场已经清理"
        )
        cause_text = str(cause).strip() or type(cause).__name__
        super().__init__(f"{cause_text}；{state}")


def _cleanup_work_path(work: Path) -> Path:
    return work.with_name(f"{work.name}.cleanup")


def _target_lock_path(work: Path) -> Path:
    return work.with_name(f"{work.name}.lock")


def _target_lock_cleanup_path(lock: Path) -> Path:
    return lock.with_name(f"{lock.name}.cleanup")


def _close_target_lock_handle(handle: int) -> BaseException | None:
    try:
        os.close(handle)
    except BaseException as error:  # noqa: BLE001 - 句柄关闭也必须保留取消与失败事实。
        return error
    return None


def _target_lock_close_failure(lock: Path, close_error: BaseException | None) -> BaseException:
    retained = OSError(f"观察目标锁保留于或需确认于 {lock}")
    return _TargetLockFailure(close_error, retained) if close_error is not None else retained


def _regular_file_identity(path: Path) -> _WorkDirectoryIdentity:
    metadata = path.lstat()
    file_attributes = getattr(metadata, "st_file_attributes", 0)
    if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or bool(file_attributes & 0x400):
        raise OSError("固定观察目标锁不是普通文件")
    if metadata.st_ino == 0:
        raise OSError("观察目标锁所在文件系统没有提供稳定文件身份")
    return metadata.st_dev, metadata.st_ino


def _regular_file_identity_at(
    path: Path,
) -> tuple[_WorkDirectoryIdentity | None, BaseException | None]:
    try:
        return _regular_file_identity(path), None
    except FileNotFoundError:
        return None, None
    except BaseException as error:  # noqa: BLE001 - 锁终态必须保留探测取消与失败。
        return None, error


def _lock_location_fact(location: str, *probe_errors: BaseException | None) -> BaseException:
    fact: BaseException = OSError(location)
    for error in reversed(tuple(error for error in probe_errors if error is not None)):
        fact = _TargetLockFailure(error, fact)
    return fact


def _acquire_target_lock(lock: Path) -> _TargetLock:
    cleanup = _target_lock_cleanup_path(lock)
    try:
        cleanup.lstat()
    except FileNotFoundError:
        pass
    except KeyboardInterrupt as error:
        raise _TargetLockFailure(error) from None
    except BaseException as error:  # noqa: BLE001 - 锁清理现场必须在建锁前确认。
        raise ToolError(
            object_name=str(cleanup),
            reason=f"固定锁清理现场无法读取（{type(error).__name__}）",
            impact="本次没有建立运行现场",
            help_text="检查该精确 .lock.cleanup 路径的权限与重解析状态后重试",
        ) from None
    else:
        fail(
            str(cleanup),
            "存在上次保留的固定锁清理现场",
            "确认对应观察任务已结束，处理这个精确 .lock.cleanup 文件后重试",
        )
    try:
        handle = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
    except KeyboardInterrupt as error:
        try:
            lock.lstat()
        except FileNotFoundError:
            retained: BaseException | None = None
        except BaseException as inspection_error:  # noqa: BLE001 - 取消后保留精确的未知锁状态。
            retained = OSError(
                f"建立锁返回前取消；锁状态无法确认（{type(inspection_error).__name__}）：{lock}"
            )
        else:
            retained = OSError(f"建立锁返回前取消；锁已保留于 {lock}")
        raise _TargetLockFailure(error, retained) from None
    except FileExistsError:
        fail(
            str(lock),
            "同一观察目标已有任务锁或上次保留的任务锁",
            "等待对应任务结束；确认没有任务运行后删除这个精确 .lock 文件并重试",
        )
    except OSError as error:
        fail(
            str(lock),
            f"观察目标锁无法建立（{type(error).__name__}）",
            "检查输出父目录的权限和重解析状态后重试",
        )
    try:
        metadata = os.fstat(handle)
    except BaseException as error:  # noqa: BLE001 - 已建立的锁必须保留准确现场。
        close_error = _close_target_lock_handle(handle)
        retained = _target_lock_close_failure(lock, close_error)
        raise _TargetLockFailure(error, retained) from None
    if not stat.S_ISREG(metadata.st_mode):
        close_error = _close_target_lock_handle(handle)
        raise _TargetLockFailure(
            OSError("新建的观察目标锁不是普通文件"),
            _target_lock_close_failure(lock, close_error),
        )
    if metadata.st_ino == 0:
        close_error = _close_target_lock_handle(handle)
        raise _TargetLockFailure(
            OSError("输出文件系统没有提供稳定的观察目标锁身份"),
            _target_lock_close_failure(lock, close_error),
        )
    return handle, (metadata.st_dev, metadata.st_ino)


def _release_target_lock(lock: Path, target_lock: _TargetLock) -> _TargetLockFailure | None:
    handle, expected_identity = target_lock
    primary: BaseException | None = None
    try:
        actual_identity = _regular_file_identity(lock)
    except BaseException as error:  # noqa: BLE001 - 锁路径状态与句柄关闭都必须保留。
        primary = error
    else:
        if actual_identity != expected_identity:
            primary = OSError(f"观察目标锁身份已经变化，已保留该路径：{lock}")
    close_error = _close_target_lock_handle(handle)
    if primary is None and close_error is not None:
        return _TargetLockFailure(
            close_error,
            OSError(f"观察目标锁保留于或需确认于 {lock}"),
        )
    if primary is not None:
        return _TargetLockFailure(
            primary,
            _target_lock_close_failure(lock, close_error),
        )

    claimed = _target_lock_cleanup_path(lock)
    try:
        os.rename(lock, claimed)
    except BaseException as error:  # noqa: BLE001 - 认领锁文件的实际结果必须保留。
        lock_identity, lock_probe_error = _regular_file_identity_at(lock)
        claimed_identity, claimed_probe_error = _regular_file_identity_at(claimed)
        if (
            lock_identity == expected_identity
            and lock_probe_error is None
            and claimed_identity is None
            and claimed_probe_error is None
        ):
            location = f"观察目标锁保留于 {lock}"
        elif (
            claimed_identity == expected_identity
            and claimed_probe_error is None
            and lock_identity is None
            and lock_probe_error is None
        ):
            location = f"观察目标锁已认领到 {claimed}"
        else:
            location = f"观察目标锁需确认于 {lock} 与 {claimed}"
        return _TargetLockFailure(
            error,
            _lock_location_fact(location, lock_probe_error, claimed_probe_error),
        )
    try:
        claimed_identity = _regular_file_identity(claimed)
    except BaseException as error:  # noqa: BLE001 - 身份不明的锁清理对象保留现场。
        return _TargetLockFailure(error, OSError(f"观察目标锁需确认于 {claimed}"))
    if claimed_identity != expected_identity:
        try:
            lock.lstat()
        except FileNotFoundError:
            try:
                os.rename(claimed, lock)
            except BaseException as restore_error:  # noqa: BLE001 - 失配对象与恢复错误都是诊断事实。
                return _TargetLockFailure(
                    OSError("被认领的观察目标锁身份已经变化"),
                    restore_error,
                )
        except BaseException as inspection_error:  # noqa: BLE001 - 不覆盖无法确认的新锁路径。
            return _TargetLockFailure(
                OSError("被认领的观察目标锁身份已经变化"),
                inspection_error,
            )
        else:
            return _TargetLockFailure(OSError(f"观察目标锁身份已经变化，已保留于 {claimed}"))
        return _TargetLockFailure(OSError(f"观察目标锁身份已经变化，已恢复该路径：{lock}"))
    try:
        claimed.unlink()
    except BaseException as error:  # noqa: BLE001 - 锁清理失败作为准确现场返回。
        remaining_identity, probe_error = _regular_file_identity_at(claimed)
        if remaining_identity is None and probe_error is None:
            location = OSError("观察目标锁已经清理")
        elif probe_error is not None:
            location = _lock_location_fact(
                f"观察目标锁状态无法确认：{claimed}",
                probe_error,
            )
        else:
            location = OSError(f"观察目标锁保留于 {claimed}")
        return _TargetLockFailure(error, location)
    return None


def _work_directory_identity(work: Path) -> _WorkDirectoryIdentity:
    metadata = work.lstat()
    file_attributes = getattr(metadata, "st_file_attributes", 0)
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or bool(file_attributes & 0x400):
        raise OSError("固定运行现场不是本工具创建的普通目录")
    if metadata.st_ino == 0:
        raise OSError("固定运行现场所在文件系统没有提供稳定目录身份")
    return metadata.st_dev, metadata.st_ino


def _retained_work_sites(work: Path) -> tuple[tuple[str, ...], BaseException | None]:
    sites: list[str] = []
    inspection_errors: list[BaseException] = []
    for path in (work, _cleanup_work_path(work)):
        try:
            path.lstat()
        except FileNotFoundError:
            continue
        except BaseException as error:  # noqa: BLE001 - 清理终态需要保留无法确认的精确路径。
            sites.append(f"{path}（状态无法确认：{type(error).__name__}）")
            inspection_errors.append(error)
        else:
            sites.append(str(path))
    cancellation = next(
        (error for error in inspection_errors if isinstance(error, KeyboardInterrupt)),
        None,
    )
    return tuple(sites), cancellation or (inspection_errors[0] if inspection_errors else None)


def _cleanup_cancellation(error: BaseException | None) -> KeyboardInterrupt | None:
    if isinstance(error, KeyboardInterrupt):
        return error
    if isinstance(error, ToolCancelledError):
        return error.cause
    if isinstance(error, DirectoryPublishedError) and isinstance(error.cause, KeyboardInterrupt):
        return error.cause
    if isinstance(error, _WorkCleanupFailure) and isinstance(error.cause, KeyboardInterrupt):
        return error.cause
    if isinstance(error, _TargetLockFailure):
        return _cleanup_cancellation(error.primary) or _cleanup_cancellation(error.secondary)
    return None


def _cleanup_has_retained_sites(error: BaseException | None) -> bool:
    return error is not None and (not isinstance(error, _WorkCleanupFailure) or bool(error.retained_sites))


def _cleanup_work_directory(
    work: Path,
    expected_identity: _WorkDirectoryIdentity | None,
) -> BaseException | None:
    if expected_identity is None:
        retained_sites, inspection_error = _retained_work_sites(work)
        if not retained_sites and inspection_error is None:
            return None
        cause = inspection_error or OSError("没有取得固定运行现场身份")
        return _WorkCleanupFailure(cause, retained_sites)

    cleanup = _cleanup_work_path(work)
    error = remove_owned_directory(work, expected_identity, cleanup)
    if error is None:
        return None
    retained_sites, inspection_error = _retained_work_sites(work)
    cause = _cleanup_cancellation(error) or _cleanup_cancellation(inspection_error) or error
    return _WorkCleanupFailure(cause, retained_sites)


def _complete_published_observation(
    output: Path,
    work: Path,
    work_identity: _WorkDirectoryIdentity | None,
) -> int:
    cleanup_error = _cleanup_work_directory(work, work_identity)
    if cleanup_error is not None:
        retained = _cleanup_has_retained_sites(cleanup_error)
        raise DirectoryPublishedError(
            object_name=str(work),
            reason=(
                "观察报告已经发布，但固定运行现场"
                f"{'无法清理' if retained else '清理调用发生异常'}"
                f"（{type(cleanup_error).__name__}）：{cleanup_error}"
            ),
            impact=(
                f"完整报告位于 {output.resolve(strict=False)}；"
                + ("运行现场仍位于指出的精确目录" if retained else "运行现场已经清理")
            ),
            help_text=(
                "保留已发布报告，关闭占用后只删除原因中指出的固定现场目录"
                if retained
                else "保留已发布报告；需要新观察时再运行命令"
            ),
            cause=_cleanup_cancellation(cleanup_error) or cleanup_error,
        ) from None
    try:
        return _published_completion(output)
    except BaseException as error:  # noqa: BLE001 - 报告已经发布，最终呈现也必须保留发布终态。
        raise DirectoryPublishedError(
            object_name=str(output.resolve(strict=False)),
            reason=f"观察报告已经发布，但最终结果呈现失败（{type(error).__name__}）",
            impact=f"完整报告位于 {output.resolve(strict=False)}；报告内容已经生效",
            help_text="保留已发布报告；终端可用后直接查看 report.json",
            cause=error,
        ) from None


def _raise_observation_failure(
    primary: BaseException,
    *,
    game_root: Path,
    work: Path,
    stop_problem: str | None = None,
    stop_error: BaseException | None = None,
    close_error: BaseException | None = None,
    lock_error: BaseException | None = None,
    cleanup_error: BaseException | None = None,
    work_owned: bool = True,
    activity: str = "NW.js 运行时观察没有完成",
) -> NoReturn:
    known_errors = (
        CdpEvaluationError,
        CdpProtocolError,
        CdpUnavailableError,
        OSError,
        subprocess.SubprocessError,
        _TargetLockFailure,
        ToolError,
    )

    def error_fact(label: str, error: BaseException) -> str:
        detail = str(error).strip() if isinstance(error, (*known_errors, KeyboardInterrupt)) else ""
        return (
            f"{label}（{type(error).__name__}）：{detail}" if detail else f"{label}（{type(error).__name__}）"
        )

    secondary: list[str] = []
    if stop_problem is not None:
        secondary.append(stop_problem)
    if stop_error is not None:
        secondary.append(error_fact("关闭本工具启动的进程失败", stop_error))
    if close_error is not None:
        secondary.append(error_fact("关闭 CDP 连接失败", close_error))
    if lock_error is not None:
        secondary.append(error_fact("释放观察目标锁失败", lock_error))
    cleanup_retained = _cleanup_has_retained_sites(cleanup_error)
    if cleanup_error is not None:
        secondary.append(
            error_fact(
                "固定运行现场无法清理" if cleanup_retained else "固定运行现场清理调用发生异常",
                cleanup_error,
            )
        )

    cancellation = next(
        (
            cause
            for error in (primary, stop_error, close_error, lock_error, cleanup_error)
            if (cause := _cleanup_cancellation(error)) is not None
        ),
        None,
    )

    work_impact = (
        "运行现场保留位置见原因"
        if cleanup_retained
        else "运行现场已经清理"
        if work_owned or cleanup_error is not None
        else "本次没有取得固定运行现场"
    )
    if isinstance(primary, KeyboardInterrupt):
        if not secondary:
            raise primary
        raise ToolCancelledError(
            object_name=str(game_root),
            reason="；".join(("使用者取消了命令", *secondary)),
            impact=f"没有发布观察报告；本工具只管理自己启动的 PID；{work_impact}",
            help_text="处理指出的进程或运行现场；需要观察报告时重新运行命令",
            cause=primary,
        ) from None
    if isinstance(primary, ToolError):
        if not secondary:
            raise primary
        details = {
            "object_name": primary.object_name,
            "reason": "；".join((primary.reason, *secondary)),
            "impact": (f"{primary.impact}；{work_impact}" if cleanup_error is not None else primary.impact),
            "help_text": primary.help_text,
        }
        if cancellation is not None:
            raise ToolCancelledError(**details, cause=cancellation) from None
        raise ToolError(**details) from None
    if not isinstance(primary, Exception):
        raise primary

    category = "NW.js 本地调试协议不可用" if isinstance(primary, CdpUnavailableError) else activity
    direct_reason = str(primary).strip() if isinstance(primary, known_errors) else ""
    details = [
        (
            f"{category}（{type(primary).__name__}）：{direct_reason}"
            if direct_reason
            else f"{category}（{type(primary).__name__}）"
        ),
        *secondary,
    ]
    failure = {
        "object_name": str(game_root),
        "reason": "；".join(details),
        "impact": f"没有发布观察报告；本工具只管理自己启动的 PID；{work_impact}",
        "help_text": "确认副本可正常启动且未禁用 remote debugging，处理指出的现场后重试",
    }
    if cancellation is not None:
        raise ToolCancelledError(**failure, cause=cancellation) from None
    raise ToolError(**failure) from None


def main() -> int:
    arguments = _parser().parse_args()
    mode = cast(str, arguments.command)
    game_argument = cast(Path, arguments.game)
    output = cast(Path, arguments.output)
    if not cast(bool, arguments.confirm_isolated_copy):
        fail(
            str(game_argument),
            "没有确认 --game 是可丢弃的隔离副本",
            "复制待验收成品后，对副本传入 --confirm-isolated-copy；不要启动源游戏或唯一成品",
        )
    startup_timeout = cast(float, arguments.startup_timeout)
    if not math.isfinite(startup_timeout) or startup_timeout <= 0:
        fail("--startup-timeout", "必须是大于 0 的有限秒数", "使用足以启动当前游戏的秒数")
    settle_ms = cast(int, getattr(arguments, "settle_ms", 0))
    duration = cast(float | None, getattr(arguments, "duration", None))
    if mode == "smoke" and settle_ms <= 0:
        fail("--settle-ms", "必须是正整数", "为每个场景渲染留出等待时间")
    if mode == "observe" and duration is not None and (not math.isfinite(duration) or duration <= 0):
        fail("--duration", "必须是大于 0 的有限秒数", "填写本次正常游玩的观察时间")

    game = discover_game(game_argument)
    game_root = require_game_root(game)
    runtime_entry = _runtime_entry(game.content_root)
    executable = require_file_within(game_root / "Game.exe", game_root, "NW.js Game.exe")
    protect_outputs([output], inputs=[game_root], forbidden_roots=[game_root], replace=False)
    work = output.resolve(strict=False).with_name(f".{output.name}.runtime")
    protect_outputs([work], inputs=[game_root], forbidden_roots=[game_root], replace=True)
    cleanup_work = _cleanup_work_path(work)
    protect_outputs([cleanup_work], inputs=[game_root], forbidden_roots=[game_root], replace=True)
    target_lock_path = _target_lock_path(work)
    protect_outputs([target_lock_path], inputs=[game_root], forbidden_roots=[game_root], replace=True)
    target_lock_cleanup = _target_lock_cleanup_path(target_lock_path)
    protect_outputs(
        [target_lock_cleanup],
        inputs=[game_root],
        forbidden_roots=[game_root],
        replace=True,
    )
    work_owned = False
    work_identity: _WorkDirectoryIdentity | None = None
    work_creation_attempted = False
    work_preexisted = False
    setup_error: BaseException | None = None
    lock_error: BaseException | None = None
    work.parent.mkdir(parents=True, exist_ok=True)
    try:
        target_lock = _acquire_target_lock(target_lock_path)
    except _TargetLockFailure as failure:
        _raise_observation_failure(
            failure.primary,
            game_root=game_root,
            work=work,
            lock_error=failure.secondary,
            work_owned=False,
        )
    try:
        try:
            cleanup_work.lstat()
        except FileNotFoundError:
            pass
        except OSError:
            fail(
                str(cleanup_work),
                "固定清理现场无法读取",
                "检查该精确路径的权限与重解析状态后重试",
            )
        else:
            fail(
                str(cleanup_work),
                "存在上次保留的固定清理现场",
                "检查该精确目录；确认不再需要后删除目录并重试",
            )
        work_creation_attempted = True
        try:
            work.mkdir()
        except FileExistsError:
            work_preexisted = True
            fail(
                str(work),
                "固定运行现场已经存在或被另一进程同时建立",
                "检查该精确目录所属的观察任务；确认不再使用后删除目录并重试",
            )
        work_owned = True
        work_identity = _work_directory_identity(work)
    except BaseException as error:  # noqa: BLE001 - 释放同目标锁后统一报告建立失败。
        setup_error = error
        if work_creation_attempted and not work_preexisted and work_identity is None:
            try:
                work_identity = _work_directory_identity(work)
            except FileNotFoundError:
                pass
            except BaseException:  # noqa: BLE001 - 无法确认身份时保留现场，仍优先报告建立失败。
                work_identity = None
            else:
                work_owned = True
    finally:
        try:
            lock_error = _release_target_lock(target_lock_path, target_lock)
        except BaseException as error:  # noqa: BLE001 - 入口 finally 必须保留释放失败并继续现场清理。
            lock_error = _TargetLockFailure(error)
    if setup_error is not None or lock_error is not None:
        work_cleanup_required = work_owned or (work_creation_attempted and not work_preexisted)
        cleanup_error = _cleanup_work_directory(work, work_identity) if work_cleanup_required else None
        if setup_error is not None:
            primary_error = setup_error
            secondary_lock_error: BaseException | None = lock_error
        else:
            assert lock_error is not None
            primary_error = lock_error.primary
            secondary_lock_error = lock_error.secondary
        _raise_observation_failure(
            primary_error,
            game_root=game_root,
            work=work,
            lock_error=secondary_lock_error,
            cleanup_error=cleanup_error,
            work_owned=work_cleanup_required,
        )
    try:
        profile = work / "profile"
        profile.mkdir()
        port = reserve_loopback_port()
        command = build_nwjs_command(executable, port=port, profile=profile)
    except BaseException as error:  # noqa: BLE001 - 从现场建立开始统一清理，并保留取消语义。
        _raise_observation_failure(
            error,
            game_root=game_root,
            work=work,
            cleanup_error=(
                _cleanup_work_directory(work, work_identity)
                if work_owned or (work_creation_attempted and not work_preexisted)
                else None
            ),
            work_owned=work_owned,
        )
    process: subprocess.Popen[bytes] | None = None
    connection: CdpConnection | None = None
    main_error: BaseException | None = None
    files: dict[str, str | bytes | Path] = {}
    try:
        process = subprocess.Popen(
            command,
            cwd=game_root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            shell=False,
        )
        listener_pid = wait_for_owned_loopback_listener(
            port,
            root_pid=process.pid,
            timeout=startup_timeout,
            process_exited=lambda: _owned_process_exited(process),
        )
        target = wait_for_page_target(
            port,
            timeout=startup_timeout,
            expected_content_root=game.content_root,
            expected_entry=runtime_entry,
            process_exited=lambda: _owned_process_exited(process),
        )
        connection = CdpConnection(target.websocket_url, timeout=max(5.0, startup_timeout))
        confirmed_listener = owned_loopback_listener_pid(port, process.pid)
        if confirmed_listener != listener_pid:
            raise CdpUnavailableError("NW.js 调试监听进程在页面连接期间发生变化")
        connection.call("Runtime.enable")
        connection.call("Page.enable")
        connection.evaluate(OBSERVER_SCRIPT)
        installation = _wait_for_observer(connection, startup_timeout)

        stats = _ObservationStats()
        scenarios: list[dict[str, object]] = []
        observation_stop: str | None = None
        startup = wait_for_runtime_start(
            connection,
            timeout=startup_timeout,
            process_exited=lambda: _owned_process_exited(process),
        )
        _record_runtime_errors(work, startup.runtime_errors, stats, phase="startup")
        current_observer = installation
        if process.poll() is None:
            startup_events, current_observer = _take_observation(connection)
            _record_events(work, startup_events, stats, phase="startup")
        startup_screenshot: str | None = None
        startup_screenshot_size: tuple[int, int] | None = None
        if startup.status != "ready" and process.poll() is None:
            startup_screenshot = "screenshots/00-startup.png"
            startup_png = _capture_screenshot(connection)
            startup_screenshot_size = decode_png_size(startup_png)
            files[startup_screenshot] = startup_png
        observation_started = time.monotonic()
        if mode == "smoke":
            if startup.status != "ready":
                reason = {
                    "runtime_error": "启动阶段发生运行时错误",
                    "timed_out": "游戏没有在时限内离开启动场景",
                    "process_exited": "游戏进程在启动完成前退出",
                }.get(startup.status, "游戏没有完成启动")
                for name in _SCENARIOS:
                    scenarios.append(
                        {
                            "name": name,
                            "status": "unverified",
                            "evidence": reason,
                            "action": {"supported": False, "reason": f"startup_{startup.status}"},
                            "event_sequence_start": _snapshot_sequence(current_observer),
                            "event_sequence_end": _snapshot_sequence(current_observer),
                            "observed_events": 0,
                            "observed_draws": 0,
                            "screenshot": None,
                            "screenshot_width": None,
                            "screenshot_height": None,
                            "observer_start": current_observer,
                            "observer_end": current_observer,
                        }
                    )
            else:
                for index, name in enumerate(_SCENARIOS, start=1):
                    transition_events, observer_start = _take_observation(connection)
                    _record_events(
                        work,
                        transition_events,
                        stats,
                        phase="transition",
                        scenario=name,
                    )
                    sequence_start = _snapshot_sequence(observer_start)
                    action = scenario_action(connection, name)
                    time.sleep(settle_ms / 1000.0)
                    events, observer_end = _take_observation(connection)
                    sequence_end = _snapshot_sequence(observer_end)
                    if sequence_end < sequence_start or any(
                        not sequence_start < _event_sequence(event) <= sequence_end for event in events
                    ):
                        raise CdpProtocolError("场景事件不在本场景观察序列边界内")
                    draws = _record_events(
                        work,
                        events,
                        stats,
                        phase="scenario",
                        scenario=name,
                    )
                    runtime_errors = _take_runtime_errors(connection)
                    _record_runtime_errors(
                        work,
                        runtime_errors,
                        stats,
                        phase="scenario",
                        scenario=name,
                    )
                    screenshot_name = f"screenshots/{index:02d}-{name}.png"
                    screenshot_png = _capture_screenshot(connection)
                    screenshot_width, screenshot_height = decode_png_size(screenshot_png)
                    files[screenshot_name] = screenshot_png
                    status, evidence = scenario_status(
                        name,
                        action,
                        draws,
                        observer_start=observer_start,
                        observer_end=observer_end,
                        screenshot_size=(screenshot_width, screenshot_height),
                    )
                    if runtime_errors:
                        status = "unverified"
                        evidence = "场景执行期间发生运行时错误"
                    scenarios.append(
                        {
                            "name": name,
                            "status": status,
                            "evidence": evidence,
                            "action": action,
                            "event_sequence_start": sequence_start,
                            "event_sequence_end": sequence_end,
                            "observed_events": len(events),
                            "observed_draws": len(draws),
                            "screenshot": screenshot_name,
                            "screenshot_width": screenshot_width,
                            "screenshot_height": screenshot_height,
                            "observer_start": observer_start,
                            "observer_end": observer_end,
                        }
                    )
                    current_observer = observer_end
                    if runtime_errors:
                        for remaining in _SCENARIOS[index:]:
                            scenarios.append(
                                {
                                    "name": remaining,
                                    "status": "unverified",
                                    "evidence": "前一场景发生运行时错误，未继续执行",
                                    "action": {"supported": False, "reason": "runtime_error"},
                                    "event_sequence_start": sequence_end,
                                    "event_sequence_end": sequence_end,
                                    "observed_events": 0,
                                    "observed_draws": 0,
                                    "screenshot": None,
                                    "screenshot_width": None,
                                    "screenshot_height": None,
                                    "observer_start": observer_end,
                                    "observer_end": observer_end,
                                }
                            )
                        break
        else:
            if startup.status != "ready":
                observation_stop = f"startup_{startup.status}"
            else:
                deadline = None if duration is None else time.monotonic() + duration
                while process.poll() is None and (deadline is None or time.monotonic() < deadline):
                    wait = 0.5 if deadline is None else min(0.5, max(0.0, deadline - time.monotonic()))
                    time.sleep(wait)
                    events, current_observer = _take_observation(connection)
                    _record_events(work, events, stats, phase="observe")
                    runtime_errors = _take_runtime_errors(connection)
                    _record_runtime_errors(work, runtime_errors, stats, phase="observe")
                    if runtime_errors:
                        observation_stop = "runtime_error"
                        break
                if observation_stop is None:
                    observation_stop = "game_closed" if process.poll() is not None else "duration_elapsed"
                if process.poll() is None:
                    events, current_observer = _take_observation(connection)
                    _record_events(work, events, stats, phase="observe")
                    _record_runtime_errors(
                        work,
                        _take_runtime_errors(connection),
                        stats,
                        phase="observe",
                    )
                    files["screenshots/final.png"] = _capture_screenshot(connection)
        if process.poll() is None:
            trailing_events, current_observer = _take_observation(connection)
            _record_events(work, trailing_events, stats, phase="trailing")
            _record_runtime_errors(
                work,
                _take_runtime_errors(connection),
                stats,
                phase="trailing",
            )
            installation = current_observer
        for name in (
            "events.jsonl",
            "draws.jsonl",
            "english-candidates.jsonl",
            "pixel-overflows.jsonl",
            "layout-measurement-unverified.jsonl",
            "font-load-review.jsonl",
            "runtime-errors.jsonl",
        ):
            path = work / name
            if not path.exists():
                path.write_text("", encoding="utf-8")
            files[name] = path
        summary: dict[str, object] = {
            "event_count": stats.event_count,
            "events_file": "events.jsonl",
            "draw_count": stats.draw_count,
            "draws_file": "draws.jsonl",
            "english_candidate_count": stats.english_count,
            "english_candidates_file": "english-candidates.jsonl",
            "pixel_overflow_count": stats.overflow_count,
            "pixel_overflows_file": "pixel-overflows.jsonl",
            "measurement_unverified_count": stats.measurement_unverified_count,
            "measurement_unverified_file": "layout-measurement-unverified.jsonl",
        }
        font_review = _font_review(stats)
        unverified_scenarios = sum(1 for item in scenarios if item.get("status") != "verified")
        actual_finding = bool(
            stats.english_count
            or stats.overflow_count
            or stats.requested_font_not_loaded_count
            or stats.runtime_error_count
            or startup.status != "ready"
        )
        has_unverified = bool(
            unverified_scenarios
            or mode == "observe"
            or not _observer_ready(installation)
            or stats.glyph_fallback_unverified
            or stats.measurement_unverified_count
        )
        if actual_finding:
            qa_status = "needs_review"
        elif has_unverified:
            qa_status = "unverified"
        else:
            qa_status = "clean"
        report: dict[str, Any] = {
            "qa_status": qa_status,
            "mode": mode,
            "engine": game.engine,
            "game_root": str(game_root),
            "content_root": str(game.content_root),
            "owned_pid": process.pid,
            "cdp_listener_pid": listener_pid,
            "page_target": target.url,
            "input_confirmed_isolated_copy": True,
            "keyboard_injection_used": False,
            "startup": {
                "status": startup.status,
                "scene": startup.scene,
                "wait_seconds": startup.wait_seconds,
                "screenshot": startup_screenshot,
                "screenshot_width": (
                    startup_screenshot_size[0] if startup_screenshot_size is not None else None
                ),
                "screenshot_height": (
                    startup_screenshot_size[1] if startup_screenshot_size is not None else None
                ),
            },
            "observer": installation,
            "scenarios": scenarios,
            "unverified_scenario_count": unverified_scenarios,
            "observation_seconds": (time.monotonic() - observation_started if mode == "observe" else None),
            "observation_stop": observation_stop,
            "runtime_error_count": stats.runtime_error_count,
            "runtime_errors_file": "runtime-errors.jsonl",
            **summary,
            "font_review": font_review,
            "interpretation": (
                "英文项只是实际绘制候选，仍需区分专名、资源名和漏译；"
                "像素越界只报告 drawText 已提供可测宽度的调用；"
                "控制符、多行文本或缺少测量 API 的 drawTextEx 只标记为 unverified；"
                "未捕获异常、引擎错误画面和启动未完成会标记为 needs_review；"
                "observe 不代表预定义场景已经验收。"
            ),
        }
        files["report.json"] = _json_text(report)
    except BaseException as error:  # noqa: BLE001 - finally 必须先停止自有进程并清理固定现场。
        main_error = error
    finally:
        stop_problem: str | None = None
        stop_error: BaseException | None = None
        if process is not None:
            try:
                stop_result = _stop_owned_process(process, connection)
            except BaseException as error:  # noqa: BLE001 - 停止错误不能覆盖观察主错误。
                stop_error = error
            else:
                stop_error = stop_result.error
                if main_error is not None or stop_error is not None or not stop_result.stopped:
                    stop_problem = "；".join(stop_result.facts) or None
        close_error: BaseException | None = None
        if connection is not None:
            try:
                connection.close()
            except BaseException as error:  # noqa: BLE001 - 连接清理错误不能覆盖观察主错误。
                close_error = error
        lifecycle_error = main_error or stop_error or close_error
        cleanup_error = None
        if lifecycle_error is not None or stop_problem is not None:
            cleanup_error = _cleanup_work_directory(work, work_identity)
        if main_error is not None:
            _raise_observation_failure(
                main_error,
                game_root=game_root,
                work=work,
                stop_problem=stop_problem,
                stop_error=stop_error,
                close_error=close_error,
                cleanup_error=cleanup_error,
                work_owned=work_owned,
            )
        if lifecycle_error is not None:
            _raise_observation_failure(
                lifecycle_error,
                game_root=game_root,
                work=work,
                stop_problem=stop_problem,
                stop_error=None if lifecycle_error is stop_error else stop_error,
                close_error=None if lifecycle_error is close_error else close_error,
                cleanup_error=cleanup_error,
                work_owned=work_owned,
            )
        if stop_problem is not None or cleanup_error is not None:
            cleanup_retained = _cleanup_has_retained_sites(cleanup_error)
            cleanup_detail = (
                "固定运行现场"
                f"{'无法清理' if cleanup_retained else '清理调用发生异常'}"
                f"（{type(cleanup_error).__name__}）：{cleanup_error}"
                if cleanup_error is not None
                else None
            )
            details = {
                "object_name": str(game_root),
                "reason": "；".join(value for value in (stop_problem, cleanup_detail) if value is not None),
                "impact": (
                    "观察结果已经取得但尚未发布；只处理本工具拥有的 PID；"
                    + ("运行现场保留位置见原因" if cleanup_retained else "运行现场已经清理")
                ),
                "help_text": (
                    "关闭指出的进程占用并处理运行现场后重新执行"
                    if cleanup_retained
                    else "处理指出的进程状态后重新执行"
                ),
            }
            cancellation = _cleanup_cancellation(cleanup_error)
            if cancellation is not None:
                raise ToolCancelledError(**details, cause=cancellation) from None
            raise ToolError(**details)
    try:
        atomic_write_directory(output, files, replace=False)
    except DirectoryPublishedError as error:
        cleanup_error = _cleanup_work_directory(work, work_identity)
        cleanup_retained = _cleanup_has_retained_sites(cleanup_error)
        cleanup_fact = (
            "；固定运行现场"
            f"{'无法清理' if cleanup_retained else '清理调用发生异常'}"
            f"（{type(cleanup_error).__name__}）：{cleanup_error}"
            if cleanup_error is not None
            else "；固定运行现场已经清理"
        )
        raise DirectoryPublishedError(
            object_name=str(output.resolve(strict=False)),
            reason=f"{error.reason}{cleanup_fact}",
            impact=(
                f"完整观察报告已经发布到 {output.resolve(strict=False)}；"
                + ("运行现场保留位置见原因" if cleanup_retained else "运行现场已经清理")
            ),
            help_text=(
                "保留已发布报告，处理原因中指出的固定现场目录"
                if cleanup_retained
                else "保留已发布报告；需要新观察时再运行命令"
            ),
            cause=_cleanup_cancellation(cleanup_error) or error.cause,
        ) from None
    except BaseException as error:  # noqa: BLE001 - 发布错误与固定现场清理结果需要同时呈现。
        _raise_observation_failure(
            error,
            game_root=game_root,
            work=work,
            cleanup_error=_cleanup_work_directory(work, work_identity),
            work_owned=work_owned,
            activity="NW.js 观察报告发布没有完成",
        )
    return _complete_published_observation(output, work, work_identity)


if __name__ == "__main__":
    run_cli(main)
