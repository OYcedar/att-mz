#!/usr/bin/env python3
"""盘点、替换和恢复 RPG Maker MV/MZ 字体引用。"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn, cast

# Skill 目录是发行资源，入口进程不得把解释器缓存写回包内。
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    OutputPublishedError,
    ToolArgumentParser,
    ToolCancelledError,
    ToolError,
    atomic_write_directory,
    atomic_write_text,
    fail,
    print_published_completion,
    protect_outputs,
    require_directory,
    require_file,
    run_cli,
    write_json,
)
from att_toolbox.fonts import (
    FontGameLockRelease,
    FontPlan,
    FontStateBinding,
    FontStateIntegrityError,
    acquire_font_game_lock,
    apply_font_plan,
    bind_font_state,
    build_font_plan,
    font_game_lock_paths,
    font_state_files,
    release_font_game_lock,
    restore_font_state,
    verify_applied_font_plan,
    verify_font_plan_source,
    verify_restored_font_state,
    write_font_apply_marker,
)
from att_toolbox.rpg import discover_game, require_game_root
from att_toolbox.translation_export import (
    projected_write_back_text,
    read_translation_export,
    translation_export_identity,
)

_BUNDLED_FONT_ROOT = Path(__file__).resolve().parents[1] / "assets" / "fonts"
_BUNDLED_FONTS = {
    "noto-sans-sc": "NotoSansCJKsc-Regular.otf",
    "noto-serif-sc": "NotoSerifCJKsc-Regular.otf",
    "lxgw-wenkai": "LXGWWenKaiGB-Regular.ttf",
}


def _add_plan_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--game", type=Path, required=True, help="完整 RPG Maker 游戏根或标准 MV www")
    parser.add_argument(
        "--font",
        required=True,
        help="noto-sans-sc、noto-serif-sc、lxgw-wenkai，或单字体 OTF/TTF 文件",
    )
    parser.add_argument("--output", type=Path, required=True, help="Review JSON 输出路径")
    parser.add_argument(
        "--coverage-text",
        action="append",
        type=Path,
        default=[],
        help="额外需要检查的 UTF-8 文本；只证明这些补充文本自身，可重复传入",
    )
    parser.add_argument(
        "--translations",
        type=Path,
        help="当前 ATT translation export JSONL；项目 WriteBack 字符覆盖的唯一权威",
    )
    parser.add_argument("--replace", action="store_true", help="替换已存在的 Review JSON")


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="递归调查并可逆替换 RPG Maker MV/MZ 字体资源。")
    commands = parser.add_subparsers(dest="command", required=True)
    inspect = commands.add_parser("inspect", help="只读生成完整引用、动态 Review 和字符覆盖报告")
    _add_plan_arguments(inspect)
    apply = commands.add_parser("apply", help="保存前后字节 state 后替换已证明资源并保留注册别名")
    _add_plan_arguments(apply)
    apply.add_argument("--state", type=Path, required=True, help="新建的可逆事务 state 目录")
    restore = commands.add_parser("restore", help="当前字节未漂移时恢复 apply 前的逐字节内容")
    restore.add_argument("--game", type=Path, required=True)
    restore.add_argument("--state", type=Path, required=True)
    restore.add_argument("--output", type=Path, required=True, help="restore 结果 JSON")
    restore.add_argument("--replace", action="store_true")
    return parser


def _coverage_paths(arguments: argparse.Namespace) -> tuple[Path, ...]:
    return tuple(require_file(path, "字符覆盖文本") for path in cast(list[Path], arguments.coverage_text))


def _visible_characters(text: str) -> str:
    return "".join(
        sorted(
            {
                character
                for character in text
                if character not in {"\ufeff", "\ufffe", "\xffff"} and not character.isspace()
            },
            key=ord,
        )
    )


@dataclass(frozen=True, slots=True)
class _CoverageProjection:
    translation_path: Path | None
    translation_identity: dict[str, JsonValue] | None
    project_text: str
    additional_paths: tuple[Path, ...]
    additional_text: str


def _coverage_projection(arguments: argparse.Namespace) -> _CoverageProjection:
    translation_argument = cast(Path | None, arguments.translations)
    if translation_argument is None:
        translation_path = None
        identity = None
        project_text = ""
    else:
        translation_path = require_file(translation_argument, "ATT Translation export JSONL")
        rows = read_translation_export(translation_path)
        project_text, _translated = projected_write_back_text(rows)
        identity = translation_export_identity(translation_path, rows)
    additional_paths = _coverage_paths(arguments)
    additional_parts: list[str] = []
    for path in additional_paths:
        try:
            additional_parts.append(path.read_text(encoding="utf-8-sig"))
        except (OSError, UnicodeError) as error:
            fail(
                str(path),
                f"额外字符覆盖文本无法按 UTF-8 读取（{type(error).__name__}）",
                "修复或重新生成该补充文本",
            )
    return _CoverageProjection(
        translation_path,
        identity,
        project_text,
        additional_paths,
        "".join(additional_parts),
    )


def _font_path(value: str) -> Path:
    bundled_name = _BUNDLED_FONTS.get(value)
    return _BUNDLED_FONT_ROOT / bundled_name if bundled_name is not None else Path(value)


def _plan(arguments: argparse.Namespace, coverage: _CoverageProjection) -> FontPlan:
    game = discover_game(cast(Path, arguments.game))
    game_root = require_game_root(game)
    font = require_file(_font_path(cast(str, arguments.font)), "替换字体")
    return build_font_plan(
        game_root=game_root,
        content_root=game.content_root,
        selected_font=font,
        coverage_characters=coverage.project_text + coverage.additional_text,
    )


def _game_root(arguments: argparse.Namespace) -> Path:
    """只解析本次字体任务用于串行化的自然游戏根。"""

    return require_game_root(discover_game(cast(Path, arguments.game)))


def _locked_plan(
    arguments: argparse.Namespace,
    coverage: _CoverageProjection,
    *,
    locked_game_root: Path,
) -> FontPlan:
    """在已持有的游戏锁内重新发现游戏并建立完整字体计划。"""

    plan = _plan(arguments, coverage)
    if plan.game_root != locked_game_root:
        fail(
            str(cast(Path, arguments.game)),
            "锁内重新发现的游戏根与本次字体任务锁不一致",
            "恢复游戏目录的稳定结构后重新运行命令",
        )
    return plan


def _font_report(
    plan: FontPlan,
    *,
    applied: bool,
    coverage: _CoverageProjection,
) -> dict[str, JsonValue]:
    assets: list[JsonValue] = [
        {
            "path": asset.relative_path,
            "size": asset.size,
            "sha256": asset.sha256,
        }
        for asset in plan.assets
    ]
    aliases: list[JsonValue] = [
        {
            "value": alias.value,
            "asset": alias.asset,
            "basis": alias.basis,
            "source": alias.source,
            "line": alias.line,
        }
        for alias in plan.aliases
    ]
    references: list[JsonValue] = [
        {
            "source": reference.source,
            "line": reference.line,
            "context": reference.context,
            "old_asset": reference.old_asset,
            "new_asset": reference.new_asset,
            "old_value": reference.old_value,
            "new_value": reference.new_value,
            "nested_location": reference.nested_location,
        }
        for reference in plan.references
    ]
    reviews: list[JsonValue] = [
        {
            "source": item.source,
            "line": item.line,
            "reason": item.reason,
            "value": item.value,
        }
        for item in plan.reviews
    ]
    mutations: list[JsonValue] = [
        {
            "path": mutation.relative_path,
            "action": "create" if mutation.original is None else "replace",
        }
        for mutation in plan.mutations
    ]
    project_characters = _visible_characters(coverage.project_text)
    additional_characters = _visible_characters(coverage.additional_text)
    missing = set(plan.coverage.missing_characters)
    project_missing = "".join(character for character in project_characters if character in missing)
    additional_missing = "".join(character for character in additional_characters if character in missing)
    translation_projection_available = bool(project_characters) and coverage.translation_identity is not None
    # Translation export 只能证明 ATT 当前导出在 WriteBack 语义下会产生哪些字符，
    # 不能单独证明它覆盖了这个游戏副本的全部实际字体消费者。完整项目结论还需要
    # 同源 Survey/coverage、实际 WriteBack 和运行副本之间的绑定；本工具不拥有这些输入。
    qa_status = "needs_review" if reviews or plan.coverage.missing_characters else "unverified"
    return {
        "qa_status": qa_status,
        "applied": applied,
        "game_root": str(plan.game_root),
        "content_root": str(plan.content_root),
        "selected_font": {
            "name": plan.selected_font.name,
            "size": plan.selected_size,
            "sha256": plan.selected_sha256,
            "glyph_count": plan.coverage.glyph_count,
        },
        "coverage": {
            "translation_export": coverage.translation_identity,
            "translation_projection_available": translation_projection_available,
            "scope": "translation_export_and_explicit_additions",
            "project_checked_characters": project_characters,
            "project_missing_characters": project_missing,
            "additional_text_files": [str(path.resolve()) for path in coverage.additional_paths],
            "additional_checked_characters": additional_characters,
            "additional_missing_characters": additional_missing,
            "checked_characters": plan.coverage.checked_characters,
            "missing_characters": plan.coverage.missing_characters,
            "missing_count": len(plan.coverage.missing_characters),
        },
        "font_assets": assets,
        "font_aliases": aliases,
        "confirmed_references": references,
        "confirmed_reference_count": len(references),
        "mutations": mutations,
        "mutation_count": len(mutations),
        "review": reviews,
        "review_count": len(reviews),
        "review_required": bool(reviews or plan.coverage.missing_characters),
        "no_op": not plan.mutations,
        "interpretation": (
            "apply 会处理 confirmed_references 指向的字体资源并保留已注册运行时别名；"
            "项目字符只从当前 ATT Translation export 投影；该投影未与同源 Survey、coverage、"
            "实际 WriteBack 和运行副本绑定，因此字体报告只给出 scoped/unverified 结论；"
            "coverage-text 只证明补充文本自身；"
            "review 只包含动态、无法解析或未证明消费者的字体事实。"
        ),
    }


def _write_apply_marker(
    plan: FontPlan,
    state: Path,
    binding: FontStateBinding,
    report: dict[str, JsonValue],
) -> bytes:
    return write_font_apply_marker(
        plan,
        state=state,
        binding=binding,
        mutation_count=cast(int, report["mutation_count"]),
        confirmed_reference_count=cast(int, report["confirmed_reference_count"]),
    )


def _raise_applied_state_failure(
    error: FontStateIntegrityError,
    *,
    plan: FontPlan,
    output: Path,
    output_published: bool,
) -> NoReturn:
    report_impact = (
        f"Review JSON {output.resolve(strict=False)} 已经发布" if output_published else "Review JSON 尚未发布"
    )
    raise ToolError(
        object_name=str(plan.game_root),
        reason=error.reason,
        impact=f"字体替换已完整生效；{error.impact}；{report_impact}",
        help_text=(
            "停止继续 apply；保留游戏目录和当前 state 路径，定位原恢复 state 的实际残留位置后人工核对"
        ),
    ) from None


def _absolute_natural(path: Path) -> Path:
    return Path(os.path.abspath(path))


def _atomic_text_paths(target: Path) -> tuple[Path, Path]:
    natural = _absolute_natural(target)
    return natural, natural.with_name(f".{natural.name}.tmp")


def _atomic_directory_paths(target: Path) -> tuple[Path, ...]:
    natural = _absolute_natural(target)
    return (
        natural,
        natural.with_name(f".{natural.name}.tmp"),
        natural.with_name(f".{natural.name}.previous"),
        natural.with_name(f".{natural.name}.tmp.cleanup"),
        natural.with_name(f".{natural.name}.previous.cleanup"),
    )


def _preflight_apply_paths(
    *,
    plan: FontPlan,
    output: Path,
    state: Path,
    coverage_inputs: tuple[Path, ...],
    replace: bool,
) -> None:
    """一次验收 apply 的显式输出、固定原子现场和同游戏任务锁。"""

    output_paths = _atomic_text_paths(output)
    state_paths = _atomic_directory_paths(state)
    lock_paths = font_game_lock_paths(plan.game_root)
    protect_outputs(
        [*output_paths, *state_paths, *lock_paths],
        inputs=[plan.game_root, plan.selected_font, *coverage_inputs],
        forbidden_roots=[plan.game_root],
        replace=True,
    )
    if output_paths[0].exists() and not replace:
        fail(str(output_paths[0]), "Review JSON 已存在", "换一个 --output，或确认可替换后传入 --replace")
    if output_paths[1].exists():
        fail(
            str(output_paths[1]),
            "存在上次保留的 Review JSON 固定临时文件",
            "检查并处理这个精确 .tmp 文件后重试",
        )
    retained_state = [path for path in state_paths if path.exists()]
    if retained_state:
        fail(
            str(state_paths[0].parent),
            f"state 或其固定原子现场已存在：{'; '.join(str(path) for path in retained_state)}",
            "为本次 apply 选择全新的 --state；先处理列出的 .tmp/.previous/.cleanup 现场",
        )


def _preflight_inspect_paths(
    *,
    plan: FontPlan,
    output: Path,
    coverage_inputs: tuple[Path, ...],
    replace: bool,
) -> None:
    """一次验收 inspect 的输出、固定临时文件和同游戏任务锁。"""

    output_paths = _atomic_text_paths(output)
    lock_paths = font_game_lock_paths(plan.game_root)
    protect_outputs(
        [*output_paths, *lock_paths],
        inputs=[plan.game_root, plan.selected_font, *coverage_inputs],
        forbidden_roots=[plan.game_root],
        replace=True,
    )
    if output_paths[0].exists() and not replace:
        fail(str(output_paths[0]), "Review JSON 已存在", "换一个 --output，或确认可替换后传入 --replace")
    if output_paths[1].exists():
        fail(
            str(output_paths[1]),
            "存在上次保留的 Review JSON 固定临时文件",
            "检查并处理这个精确 .tmp 文件后重试",
        )


def _preflight_restore_paths(
    *,
    game_root: Path,
    state: Path,
    output: Path,
    replace: bool,
) -> None:
    output_paths = _atomic_text_paths(output)
    marker_paths = _atomic_text_paths(state / "restored.json")
    lock_paths = font_game_lock_paths(game_root)
    protect_outputs(
        [*output_paths, *marker_paths, *lock_paths],
        inputs=[game_root],
        forbidden_roots=[game_root],
        replace=True,
    )
    protect_outputs(
        [output_paths[0], *lock_paths],
        inputs=[state],
        replace=True,
    )
    if output_paths[0].exists() and not replace:
        fail(
            str(output_paths[0]), "restore 结果 JSON 已存在", "换一个 --output，或确认可替换后传入 --replace"
        )
    retained = [path for path in (output_paths[1], marker_paths[0], marker_paths[1]) if path.exists()]
    if retained:
        fail(
            str(state),
            f"restore 结果或固定临时现场已存在：{'; '.join(str(path) for path in retained)}",
            "核对已有 restore 结果；处理列出的精确路径后再运行",
        )


def _cancel_cause(error: BaseException) -> KeyboardInterrupt | None:
    if isinstance(error, KeyboardInterrupt):
        return error
    cause = getattr(error, "cause", None)
    return cause if isinstance(cause, KeyboardInterrupt) else None


def _raise_lock_release_failure(
    *,
    game_root: Path,
    release: FontGameLockRelease,
    primary: BaseException | None,
) -> NoReturn:
    error_text = "；".join(type(error).__name__ for error in release.errors)
    lock_facts: list[str] = []
    if release.retained_sites:
        lock_facts.append(f"任务锁现场保留于 {' 与 '.join(str(path) for path in release.retained_sites)}")
    if release.uncertain_sites:
        lock_facts.append(f"任务锁状态需确认于 {' 与 '.join(str(path) for path in release.uncertain_sites)}")
    lock_fact = "；".join(lock_facts) if lock_facts else "字体任务锁已经清理"
    needs_lock_action = bool(release.retained_sites or release.uncertain_sites)
    lock_help = (
        "确认没有字体任务运行后处理上述精确锁路径" if needs_lock_action else "任务锁已经清理，无需处理锁路径"
    )
    if isinstance(primary, ToolError):
        reason = f"{primary.reason}；字体任务锁释放另发生 {error_text}"
        impact = f"{primary.impact}；{lock_fact}"
        help_text = f"{primary.help_text}；{lock_help}"
        object_name = primary.object_name
    elif primary is not None:
        reason = f"字体操作发生 {type(primary).__name__}；任务锁释放另发生 {error_text}"
        impact = f"字体操作未完整返回；{lock_fact}"
        help_text = f"保留游戏、state 和结果；{lock_help}"
        object_name = str(game_root)
    else:
        reason = f"最终验收完成后字体任务锁释放发生 {error_text}"
        impact = f"字体操作主流程已经完成；{lock_fact}"
        help_text = f"按本次结果继续业务；{lock_help}"
        object_name = str(game_root)
    cancel = (_cancel_cause(primary) if primary is not None else None) or next(
        (cause for error in release.errors if (cause := _cancel_cause(error)) is not None),
        None,
    )
    if cancel is not None:
        raise ToolCancelledError(
            object_name=object_name,
            reason=reason,
            impact=impact,
            help_text=help_text,
            cause=cancel,
        ) from None
    raise ToolError(
        object_name=object_name,
        reason=reason,
        impact=impact,
        help_text=help_text,
    ) from None


def _run_with_font_game_lock(game_root: Path, operation: Callable[[], int]) -> int:
    lock = acquire_font_game_lock(game_root)
    primary: BaseException | None = None
    result = 1
    try:
        result = operation()
    except BaseException as error:  # noqa: BLE001 - 主结果与锁清理事实必须同时保留。
        primary = error
    try:
        release = release_font_game_lock(lock)
    except BaseException as error:  # noqa: BLE001 - 锁边界异常也必须保留主业务事实。
        release = FontGameLockRelease((error,), (), (lock.path, lock.cleanup_path))
    if release.errors or release.retained_sites or release.uncertain_sites:
        _raise_lock_release_failure(
            game_root=game_root,
            release=release,
            primary=primary,
        )
    if primary is not None:
        cancellation = _cancel_cause(primary)
        if cancellation is not None:
            if isinstance(primary, ToolError):
                object_name = primary.object_name
                reason = primary.reason
                impact = f"{primary.impact}；字体任务锁已经清理"
                help_text = primary.help_text
            else:
                object_name = str(game_root)
                reason = "使用者取消了锁内字体任务"
                impact = "游戏、state 与结果保留在取消点可核对的实际状态；字体任务锁已经清理"
                help_text = "核对 state/status.json、显式结果和游戏自然路径后决定 restore 或重新运行"
            raise ToolCancelledError(
                object_name=object_name,
                reason=reason,
                impact=impact,
                help_text=help_text,
                cause=cancellation,
            ) from None
        raise primary
    return result


def _run_inspect(arguments: argparse.Namespace) -> int:
    coverage = _coverage_projection(arguments)
    game_root = _game_root(arguments)
    output = cast(Path, arguments.output)
    coverage_inputs = coverage.additional_paths
    if coverage.translation_path is not None:
        coverage_inputs = (coverage.translation_path, *coverage_inputs)
    replace = cast(bool, arguments.replace)

    def locked_inspect() -> int:
        plan = _locked_plan(arguments, coverage, locked_game_root=game_root)
        _preflight_inspect_paths(
            plan=plan,
            output=output,
            coverage_inputs=coverage_inputs,
            replace=replace,
        )
        write_json(output, _font_report(plan, applied=False, coverage=coverage), replace=replace)
        return 0

    result = _run_with_font_game_lock(game_root, locked_inspect)
    published_output = _absolute_natural(output)
    print_published_completion(
        f"字体调查完成：{published_output}",
        object_name=str(published_output),
        impact=f"Review JSON {published_output} 已经发布；游戏文件保持 inspect 前字节",
        help_text="直接使用已发布的 Review JSON；终端提示无需补写",
    )
    return result


def _run_apply(arguments: argparse.Namespace) -> int:
    coverage = _coverage_projection(arguments)
    game_root = _game_root(arguments)
    output = cast(Path, arguments.output)
    state = cast(Path, arguments.state)
    coverage_inputs = coverage.additional_paths
    if coverage.translation_path is not None:
        coverage_inputs = (coverage.translation_path, *coverage_inputs)
    replace = cast(bool, arguments.replace)
    had_mutations = False

    def locked_apply() -> int:
        nonlocal had_mutations
        plan = _locked_plan(arguments, coverage, locked_game_root=game_root)
        had_mutations = bool(plan.mutations)
        _preflight_apply_paths(
            plan=plan,
            output=output,
            state=state,
            coverage_inputs=coverage_inputs,
            replace=replace,
        )
        verify_font_plan_source(plan)
        if not plan.mutations:
            write_json(output, _font_report(plan, applied=False, coverage=coverage), replace=replace)
            return 0
        try:
            atomic_write_directory(state, font_state_files(plan), replace=False)
        except OutputPublishedError as error:
            raise OutputPublishedError(
                object_name=str(state.resolve(strict=False)),
                reason=error.reason,
                impact=(f"恢复 state 已经完整建立在 {state.resolve(strict=False)}；目标游戏尚未开始字体替换"),
                help_text=(
                    "使用这个 state 执行 restore 核对并记录原字节；需要重新 apply 时选择新的 --state 输出目录"
                ),
                cause=error.cause,
            ) from None
        try:
            binding = bind_font_state(plan, state=state)
        except FontStateIntegrityError as error:
            raise ToolError(
                object_name=error.object_name,
                reason=error.reason,
                impact=f"目标游戏尚未开始字体替换；{error.impact}",
                help_text=("保留游戏目录和当前 state 路径，重新 inspect 并为下次 apply 选择新的 state 目录"),
            ) from None
        apply_font_plan(plan, state=state, binding=binding)
        report = _font_report(plan, applied=True, coverage=coverage)
        marker_body: bytes | None = None
        output_published = False
        try:
            marker_body = _write_apply_marker(plan, state, binding, report)
            write_json(output, report, replace=replace)
            output_published = True
        except FontStateIntegrityError as error:
            _raise_applied_state_failure(
                error,
                plan=plan,
                output=output,
                output_published=output_published,
            )
        except OutputPublishedError as error:
            if marker_body is not None:
                try:
                    verify_applied_font_plan(
                        plan,
                        state=state,
                        binding=binding,
                        applied_marker=marker_body,
                    )
                except FontStateIntegrityError as state_error:
                    _raise_applied_state_failure(
                        state_error,
                        plan=plan,
                        output=output,
                        output_published=True,
                    )
            raise OutputPublishedError(
                object_name=error.object_name,
                reason=error.reason,
                impact=(
                    f"字体替换已完整生效，恢复所需 state 保留在 {state.resolve(strict=False)}；{error.impact}"
                ),
                help_text=error.help_text,
                cause=error.cause,
            ) from None
        except ToolError as error:
            if marker_body is not None:
                try:
                    verify_applied_font_plan(
                        plan,
                        state=state,
                        binding=binding,
                        applied_marker=marker_body,
                    )
                except FontStateIntegrityError as state_error:
                    _raise_applied_state_failure(
                        state_error,
                        plan=plan,
                        output=output,
                        output_published=output_published,
                    )
            cancellation = _cancel_cause(error)
            if cancellation is not None:
                raise ToolCancelledError(
                    object_name=error.object_name,
                    reason=error.reason,
                    impact=(
                        f"字体替换已完整生效，恢复所需 state 保留在 {state.resolve(strict=False)}；"
                        f"{error.impact}"
                    ),
                    help_text=error.help_text,
                    cause=cancellation,
                ) from None
            raise ToolError(
                object_name=error.object_name,
                reason=error.reason,
                impact=(
                    f"字体替换已完整生效，恢复所需 state 保留在 {state.resolve(strict=False)}；"
                    "applied 标记或 Review JSON 未完整发布"
                ),
                help_text="先用现有 state restore，或处理输出问题后人工保存报告",
            ) from None
        try:
            verify_applied_font_plan(
                plan,
                state=state,
                binding=binding,
                applied_marker=marker_body,
            )
        except FontStateIntegrityError as error:
            _raise_applied_state_failure(
                error,
                plan=plan,
                output=output,
                output_published=True,
            )
        return 0

    result = _run_with_font_game_lock(game_root, locked_apply)
    published_output = _absolute_natural(output)
    if had_mutations:
        published_state = _absolute_natural(state)
        print_published_completion(
            f"字体替换完成：{published_output}；恢复状态：{published_state}",
            object_name=str(published_output),
            impact=(f"字体替换、Review JSON {published_output} 与恢复 state {published_state} 均已完整生效"),
            help_text="按已发布的 Review JSON 继续审核；需要撤销时使用该恢复 state",
        )
    else:
        print_published_completion(
            f"字体检查完成，无需写入：{published_output}",
            object_name=str(published_output),
            impact=f"Review JSON {published_output} 已经发布；游戏和 state 均未修改",
            help_text="直接使用已发布的 Review JSON；终端提示无需补写",
        )
    return result


def _run_restore(arguments: argparse.Namespace) -> int:
    game = discover_game(cast(Path, arguments.game))
    game_root = require_game_root(game)
    state_argument = cast(Path, arguments.state)
    output = cast(Path, arguments.output)
    replace = cast(bool, arguments.replace)

    def locked_restore() -> int:
        state = require_directory(state_argument, "字体事务 state")
        _preflight_restore_paths(
            game_root=game_root,
            state=state,
            output=output,
            replace=replace,
        )
        restored = restore_font_state(game_root=game_root, state=state)
        report: dict[str, JsonValue] = {
            "complete": True,
            "restored": True,
            "game_root": str(game_root),
            "state": str(state),
            "restored_entry_count": restored,
        }
        try:
            atomic_write_text(
                state / "restored.json",
                json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
                replace=False,
            )
            write_json(output, report, replace=replace)
        except OutputPublishedError as error:
            raise OutputPublishedError(
                object_name=error.object_name,
                reason=error.reason,
                impact=f"目标游戏已经逐字节恢复；{error.impact}",
                help_text=error.help_text,
                cause=error.cause,
            ) from None
        except ToolError as error:
            cancellation = _cancel_cause(error)
            if cancellation is not None:
                raise ToolCancelledError(
                    object_name=error.object_name,
                    reason=error.reason,
                    impact=f"目标游戏已经逐字节恢复；{error.impact}",
                    help_text=error.help_text,
                    cause=cancellation,
                ) from None
            raise ToolError(
                object_name=error.object_name,
                reason=error.reason,
                impact="目标游戏已经逐字节恢复；state 标记或结果 JSON 未完整发布",
                help_text="核对游戏摘要后处理结果文件",
            ) from None
        verify_restored_font_state(game_root=game_root, state=state)
        return 0

    result = _run_with_font_game_lock(game_root, locked_restore)
    published_output = _absolute_natural(output)
    published_state = _absolute_natural(state_argument)
    print_published_completion(
        f"字体原始字节已恢复：{published_output}",
        object_name=str(published_output),
        impact=(
            f"游戏文件已经恢复为 apply 前字节；state {published_state} 已记录 restored；"
            f"restore 结果 {published_output} 已经发布"
        ),
        help_text="按已发布结果继续使用已恢复的游戏副本；终端提示无需补写",
    )
    return result


def main() -> int:
    arguments = _parser().parse_args()
    command = cast(str, arguments.command)
    if command == "inspect":
        return _run_inspect(arguments)
    if command == "apply":
        return _run_apply(arguments)
    if command == "restore":
        return _run_restore(arguments)
    fail("字体命令", "未知子命令", "运行 --help 查看 inspect/apply/restore")


if __name__ == "__main__":
    run_cli(main)
