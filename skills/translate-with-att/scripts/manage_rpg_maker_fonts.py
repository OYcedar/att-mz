#!/usr/bin/env python3
"""盘点、替换和恢复 RPG Maker MV/MZ 字体引用。"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import cast

# Skill 目录是发行资源，入口进程不得把解释器缓存写回包内。
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    ToolError,
    atomic_write_directory,
    atomic_write_text,
    fail,
    protect_outputs,
    require_directory,
    require_file,
    run_cli,
    write_json,
)
from att_toolbox.fonts import (
    FontPlan,
    apply_font_plan,
    build_font_plan,
    font_state_files,
    restore_font_state,
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
    translation_projection_available = (
        bool(project_characters) and coverage.translation_identity is not None
    )
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


def _write_apply_marker(state: Path, report: dict[str, JsonValue]) -> None:
    marker = state / "applied.json"
    atomic_write_text(
        marker,
        json.dumps(
            {
                "applied": True,
                "mutation_count": report["mutation_count"],
                "confirmed_reference_count": report["confirmed_reference_count"],
            },
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n",
        replace=False,
    )


def _run_inspect(arguments: argparse.Namespace) -> int:
    coverage = _coverage_projection(arguments)
    plan = _plan(arguments, coverage)
    output = cast(Path, arguments.output)
    coverage_inputs = coverage.additional_paths
    if coverage.translation_path is not None:
        coverage_inputs = (coverage.translation_path, *coverage_inputs)
    protect_outputs(
        [output],
        inputs=[plan.game_root, plan.selected_font, *coverage_inputs],
        forbidden_roots=[plan.game_root],
        replace=cast(bool, arguments.replace),
    )
    write_json(
        output,
        _font_report(plan, applied=False, coverage=coverage),
        replace=cast(bool, arguments.replace),
    )
    print(f"字体调查完成：{output.resolve(strict=False)}")
    return 0


def _run_apply(arguments: argparse.Namespace) -> int:
    coverage = _coverage_projection(arguments)
    plan = _plan(arguments, coverage)
    output = cast(Path, arguments.output)
    state = cast(Path, arguments.state)
    coverage_inputs = coverage.additional_paths
    if coverage.translation_path is not None:
        coverage_inputs = (coverage.translation_path, *coverage_inputs)
    protect_outputs(
        [output],
        inputs=[plan.game_root, plan.selected_font, *coverage_inputs],
        forbidden_roots=[plan.game_root],
        replace=cast(bool, arguments.replace),
    )
    protect_outputs(
        [state],
        inputs=[plan.game_root, plan.selected_font, *coverage_inputs],
        forbidden_roots=[plan.game_root],
        replace=False,
    )
    if not plan.mutations:
        report = _font_report(
            plan,
            applied=False,
            coverage=coverage,
        )
        write_json(output, report, replace=cast(bool, arguments.replace))
        print(f"字体检查完成，无需写入：{output.resolve(strict=False)}")
        return 0
    atomic_write_directory(state, font_state_files(plan), replace=False)
    apply_font_plan(plan, state=state)
    report = _font_report(
        plan,
        applied=True,
        coverage=coverage,
    )
    try:
        _write_apply_marker(state, report)
        write_json(output, report, replace=cast(bool, arguments.replace))
    except ToolError as error:
        raise ToolError(
            object_name=error.object_name,
            reason=error.reason,
            impact=(
                f"字体替换已完整生效，恢复所需 state 保留在 {state.resolve(strict=False)}；"
                "applied 标记或 Review JSON 未完整发布"
            ),
            help_text="不要再次 apply；先用现有 state restore，或处理输出问题后人工保存报告",
        ) from None
    print(f"字体替换完成：{output.resolve(strict=False)}；恢复状态：{state.resolve(strict=False)}")
    return 0


def _run_restore(arguments: argparse.Namespace) -> int:
    game = discover_game(cast(Path, arguments.game))
    game_root = require_game_root(game)
    state = require_directory(cast(Path, arguments.state), "字体事务 state")
    output = cast(Path, arguments.output)
    protect_outputs(
        [output],
        inputs=[game_root, state],
        forbidden_roots=[game_root, state],
        replace=cast(bool, arguments.replace),
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
        write_json(output, report, replace=cast(bool, arguments.replace))
    except ToolError as error:
        raise ToolError(
            object_name=error.object_name,
            reason=error.reason,
            impact="目标游戏已经逐字节恢复；state 标记或结果 JSON 未完整发布",
            help_text="不要再次 restore；核对游戏摘要后处理结果文件",
        ) from None
    print(f"字体原始字节已恢复：{output.resolve(strict=False)}")
    return 0


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
