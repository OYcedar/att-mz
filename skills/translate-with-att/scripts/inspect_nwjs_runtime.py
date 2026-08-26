#!/usr/bin/env python3
"""在隔离游戏副本中通过 CDP 记录 RPG Maker NW.js 实际绘制文本。"""

from __future__ import annotations

import argparse
import base64
import contextlib
import json
import math
import os
import shutil
import subprocess
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, TextIO, cast

# Skill 目录是发行资源，入口进程不得把解释器缓存写回包内。
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    ToolArgumentParser,
    ToolError,
    atomic_write_directory,
    fail,
    protect_outputs,
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


def _stop_owned_process(
    process: subprocess.Popen[bytes], connection: CdpConnection | None
) -> tuple[bool, str | None]:
    """只关闭本工具启动并能由 Toolhelp32 证明的进程树。"""

    def descendants_stopped() -> bool:
        if os.name != "nt":
            return True
        return not (process_tree_pids(process.pid, require_root=False) - {process.pid})

    if process.poll() is not None:
        return (
            (True, None)
            if descendants_stopped()
            else (False, "本工具启动的 NW.js 后代进程仍在运行，工具未终止这些 PID")
        )
    if connection is not None:
        try:
            connection.call("Browser.close")
        except (CdpProtocolError, CdpUnavailableError, OSError):
            with contextlib.suppress(CdpProtocolError, CdpUnavailableError, OSError):
                connection.evaluate("window.nw && nw.App ? (nw.App.quit(), true) : false")
    try:
        process.wait(timeout=4.0)
        return (
            (True, None)
            if descendants_stopped()
            else (False, "本工具启动的 NW.js 后代进程仍在运行，工具未终止这些 PID")
        )
    except subprocess.TimeoutExpired:
        pass
    try:
        process.terminate()
        process.wait(timeout=4.0)
        return (
            (True, None)
            if descendants_stopped()
            else (False, "本工具启动的 NW.js 后代进程仍在运行，工具未终止这些 PID")
        )
    except (OSError, subprocess.TimeoutExpired):
        return False, "本工具启动的 NW.js PID 无法在观察结束后退出"


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


def _cleanup_work_directory(work: Path) -> BaseException | None:
    if not work.exists():
        return None
    try:
        shutil.rmtree(work)
    except BaseException as error:  # noqa: BLE001 - 清理结果必须与观察结果分开报告。
        return error
    return None


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
    protect_outputs([work], inputs=[game_root], forbidden_roots=[game_root], replace=False)
    if work.exists():
        fail(str(work), "存在上次未清理的 NW.js 观察现场", "检查现场后删除该精确目录，再重新运行")
    work.mkdir(parents=True)
    profile = work / "profile"
    profile.mkdir()
    port = reserve_loopback_port()
    command = build_nwjs_command(executable, port=port, profile=profile)
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
        candidate_connection = CdpConnection(target.websocket_url, timeout=max(5.0, startup_timeout))
        try:
            confirmed_listener = owned_loopback_listener_pid(port, process.pid)
            if confirmed_listener != listener_pid:
                raise CdpUnavailableError("NW.js 调试监听进程在页面连接期间发生变化")
        except BaseException:
            candidate_connection.close()
            raise
        connection = candidate_connection
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
                try:
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
                except KeyboardInterrupt:
                    observation_stop = "keyboard_interrupt"
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
    except (CdpProtocolError, CdpUnavailableError, OSError, subprocess.SubprocessError) as error:
        main_error = error
    finally:
        stop_problem: str | None = None
        if process is not None:
            _, stop_problem = _stop_owned_process(process, connection)
        if connection is not None:
            connection.close()
        cleanup_error = (
            _cleanup_work_directory(work) if main_error is not None or stop_problem is not None else None
        )
        if main_error is not None:
            category = (
                "NW.js 本地调试协议不可用"
                if isinstance(main_error, CdpUnavailableError)
                else "NW.js 运行时观察没有完成"
            )
            direct_reason = str(main_error).strip()
            details = [f"{category}：{direct_reason}" if direct_reason else category]
            if stop_problem is not None:
                details.append(stop_problem)
            if cleanup_error is not None:
                details.append("固定运行现场无法清理")
            raise ToolError(
                object_name=str(game_root),
                reason="；".join(details),
                impact=(
                    "没有发布观察报告；本工具只管理自己启动的 PID；"
                    + (f"运行现场仍可能位于 {work}" if cleanup_error is not None else "运行现场已经清理")
                ),
                help_text="确认副本可正常启动且未禁用 remote debugging，处理指出的现场后重试",
            ) from None
        if stop_problem is not None or cleanup_error is not None:
            raise ToolError(
                object_name=str(game_root),
                reason="；".join(
                    value
                    for value in (stop_problem, "固定运行现场无法清理" if cleanup_error else None)
                    if value is not None
                ),
                impact="观察结果已经取得但尚未发布；只处理本工具拥有的 PID 和固定运行现场",
                help_text="关闭指出的进程占用并处理运行现场后重新执行",
            )
    try:
        atomic_write_directory(output, files, replace=False)
    except BaseException:
        _ = _cleanup_work_directory(work)
        raise
    cleanup_error = _cleanup_work_directory(work)
    if cleanup_error is not None:
        raise ToolError(
            object_name=str(work),
            reason="观察报告已经发布，但固定运行现场无法清理",
            impact=f"完整报告位于 {output.resolve(strict=False)}；运行现场仍位于指出的精确目录",
            help_text="保留已发布报告，关闭占用后只删除指出的 .runtime 目录",
        )
    print(f"NW.js 观察完成：{output.resolve(strict=False)}")
    return 0


if __name__ == "__main__":
    run_cli(main)
