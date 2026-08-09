#!/usr/bin/env python3
"""分析 MV 对话首行的姓名 marker，并从 Agent 审核结果写规则。"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    display_path,
    fail,
    protect_outputs,
    read_json_object,
    require_list,
    run_cli,
    strings_only,
    toml_string,
    validate_object_keys,
    write_json_with_optional_text,
)
from att_toolbox.rpg import discover_game, iter_dialogue_first_lines

_PREFIX = re.compile(
    r"\A(?P<marker>(?:\\[A-Za-z]+)?<(?P<angle>[^>\r\n]*)>|\\[A-Za-z]+\[(?P<bracket>[^\]\r\n]*)\])"
)
_NAMED_CAPTURE = re.compile(r"\(\?<([A-Za-z_][A-Za-z0-9_]*)>")
_UNRECOGNIZED_FORMS: tuple[tuple[str, re.Pattern[str], str], ...] = (
    ("corner_brackets", re.compile(r"\A【[^】\r\n]{1,40}】"), "【{text}】"),
    ("square_brackets", re.compile(r"\A\[[^\]\r\n]{1,40}\]"), "[{text}]"),
    ("japanese_quotes", re.compile(r"\A「[^」\r\n]{1,40}」"), "「{text}」"),
    ("colon_label", re.compile(r"\A[^\s:：\r\n]{1,40}[:：]"), "{text}："),
)


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(
        description="统计 MV 标准对话首行的 marker；只在提供 Agent 审核决定时写 dialogue rules。"
    )
    parser.add_argument("--game", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="候选 JSON 路径")
    parser.add_argument("--decisions", type=Path, help='Agent 审核 JSON：{"patterns": [...]}')
    parser.add_argument("--rules-output", type=Path, help="审核后的 dialogue-rules.toml")
    parser.add_argument("--replace", action="store_true")
    return parser


def _load_patterns(path: Path) -> list[str]:
    root = read_json_object(path, "MV 姓名规则审核文件")
    validate_object_keys(root, str(path), {"patterns"})
    raw = require_list(root.get("patterns"), str(path), "patterns")
    patterns = strings_only(raw, str(path), "patterns")
    if len(set(patterns)) != len(patterns):
        fail(str(path), "patterns 包含重复模式", "删除重复模式")
    for number, pattern in enumerate(patterns, start=1):
        if not pattern:
            fail(str(path), f"第 {number} 个 pattern 为空", "删除空模式或写入明确模式")
        captures = _NAMED_CAPTURE.findall(pattern)
        if captures != ["speaker"]:
            fail(
                str(path),
                f"第 {number} 个 pattern 的命名捕获不是唯一 speaker",
                "使用且只使用一个 (?<speaker>...) 命名捕获，再交给 ATT 的 PCRE2 编译边界验收",
            )
    return patterns


def _rules_toml(patterns: list[str]) -> str:
    if not patterns:
        return "rule = []\n"
    chunks = [f"[[rule]]\npattern = {toml_string(pattern)}\n" for pattern in patterns]
    return "\n".join(chunks)


def _single_line_prefix(value: str) -> str:
    cleaned = "".join(" " if character in "\r\n" else character for character in value)
    cleaned = "".join(character for character in cleaned if ord(character) >= 0x20 and character != "\x7f")
    return cleaned[:80]


def _unrecognized_shape(value: str) -> str:
    for name, expression, shape in _UNRECOGNIZED_FORMS:
        if expression.match(value) is not None:
            return f"{name}:{shape}"
    if value.startswith("\\"):
        return "leading_backslash"
    if value and value[0].isascii() and value[0].isalnum():
        return "leading_ascii_text"
    if value and value[0].isalnum():
        return "leading_non_ascii_text"
    if value:
        return "leading_symbol"
    return "empty_first_line"


def _analyze(args: argparse.Namespace) -> int:
    decisions_path = cast(Path | None, args.decisions)
    rules_output = cast(Path | None, args.rules_output)
    if (decisions_path is None) != (rules_output is None):
        fail("命令行参数", "--decisions 与 --rules-output 必须同时提供", "同时提供审核 JSON 和规则输出路径")
    patterns = _load_patterns(decisions_path) if decisions_path is not None else None
    game = discover_game(args.game)
    targets = [args.output] if rules_output is None else [args.output, rules_output]
    inputs = [] if decisions_path is None else [decisions_path]
    protect_outputs(
        targets,
        inputs=inputs,
        forbidden_roots=[game.supplied_root, game.content_root],
        replace=args.replace,
    )
    if game.engine != "mv":
        fail(str(game.content_root), "当前游戏不是 MV", "MZ 使用原生 Speaker，不要制作 MV 姓名投影规则")
    grouped: dict[str, dict[str, JsonValue]] = {}
    unrecognized_grouped: dict[str, dict[str, JsonValue]] = {}
    unrecognized = 0
    total = 0
    for command, first_line in iter_dialogue_first_lines(game.content_root):
        total += 1
        match = _PREFIX.match(first_line)
        if match is None:
            unrecognized += 1
            shape = _unrecognized_shape(first_line)
            entry = unrecognized_grouped.setdefault(
                shape,
                {"shape": shape, "occurrences": 0, "locations": [], "prefix_samples": []},
            )
            entry["occurrences"] = cast(int, entry["occurrences"]) + 1
            cast(list[JsonValue], entry["locations"]).append(command.location)
            samples = cast(list[JsonValue], entry["prefix_samples"])
            sample = _single_line_prefix(first_line)
            if sample not in samples and len(samples) < 5:
                samples.append(sample)
            continue
        marker = match.group("marker")
        speaker = match.group("angle") if match.group("angle") is not None else match.group("bracket")
        speaker_group = "angle" if match.group("angle") is not None else "bracket"
        speaker_start, speaker_end = match.span(speaker_group)
        marker_start = match.start("marker")
        relative_start = speaker_start - marker_start
        relative_end = speaker_end - marker_start
        shape = f"{marker[:relative_start]}{{speaker}}{marker[relative_end:]}"
        entry = grouped.setdefault(
            shape,
            {
                "shape": shape,
                "occurrences": 0,
                "non_blank_speakers": 0,
                "blank_speakers": 0,
                "body_after_marker": 0,
                "speaker_contains_backslash": 0,
                "backslash_bracket_shape": 0,
                "locations": [],
                "speaker_examples": [],
            },
        )
        entry["occurrences"] = cast(int, entry["occurrences"]) + 1
        if speaker.strip():
            entry["non_blank_speakers"] = cast(int, entry["non_blank_speakers"]) + 1
        else:
            entry["blank_speakers"] = cast(int, entry["blank_speakers"]) + 1
        if first_line[match.end() :].strip():
            entry["body_after_marker"] = cast(int, entry["body_after_marker"]) + 1
        if "\\" in speaker:
            entry["speaker_contains_backslash"] = cast(int, entry["speaker_contains_backslash"]) + 1
        if match.group("bracket") is not None:
            entry["backslash_bracket_shape"] = cast(int, entry["backslash_bracket_shape"]) + 1
        locations = cast(list[JsonValue], entry["locations"])
        locations.append(command.location)
        examples = cast(list[JsonValue], entry["speaker_examples"])
        if speaker not in examples and len(examples) < 5:
            examples.append(speaker)

    candidates = [grouped[key] for key in sorted(grouped)]
    for candidate in candidates:
        conflicts: list[JsonValue] = []
        for field, kind in (
            ("blank_speakers", "blank_speaker"),
            ("body_after_marker", "marker_and_body_on_same_line"),
            ("speaker_contains_backslash", "speaker_contains_control_like_text"),
            ("backslash_bracket_shape", "ordinary_control_code_shape_possible"),
        ):
            count = candidate.get(field)
            if isinstance(count, int) and count:
                conflicts.append({"kind": kind, "occurrences": count})
        candidate["conflict_facts"] = conflicts
    unrecognized_candidates = [unrecognized_grouped[key] for key in sorted(unrecognized_grouped)]
    result: dict[str, JsonValue] = {
        "engine": "mv",
        "content_root": str(game.content_root),
        "dialogue_blocks": total,
        "recognized_prefix_blocks": total - unrecognized,
        "unrecognized_prefix_blocks": unrecognized,
        "unrecognized_prefixes": unrecognized_candidates,
        "candidates": candidates,
        "decision_required": True,
        "decision_format": {"patterns": ["\\A\\\\N<(?<speaker>[^>]*)>"]},
        "notes": [
            "候选只证明对话首行存在 marker 外形，不证明插件消费协议。",
            "unrecognized_prefixes 是反例和其他可能协议的机械汇总，不会自动生成规则。",
            "正例、形似反例和未翻译 WriteBack 逐字往返验收仍由 Agent 和 ATT 负责。",
        ],
    }
    write_json_with_optional_text(
        args.output,
        result,
        text_path=rules_output,
        text=_rules_toml(patterns) if patterns is not None else None,
        replace=args.replace,
    )
    print(
        f"已检查 {total} 个 MV 对话块，找到 {len(candidates)} 类姓名 marker 候选：{display_path(args.output)}"
    )
    if patterns is not None and rules_output is not None:
        print(f"已按 Agent 审核写入 {len(patterns)} 条规则：{display_path(rules_output)}")
    else:
        print("下一步：核对活动姓名框插件、正例和反例，再提供 decisions JSON。")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _analyze(parsed))
