#!/usr/bin/env python3
"""从最终 Extract 的 Manual 语料统计自定义 Placeholder 候选。"""

from __future__ import annotations

import argparse
import json
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
    read_manual,
    require_list,
    run_cli,
    strings_only,
    toml_string,
    validate_object_keys,
    write_json_with_optional_text,
)

_CONTROL_LIKE = re.compile(r"\\(?:[A-Za-z]+(?:\[[^\]\r\n]*\]|<[^>\r\n]*>)?|[\\{}$.|!><^])")
_CUSTOM_FORMS: tuple[tuple[str, re.Pattern[str], str], ...] = (
    (
        "backslash_bracket",
        re.compile(r"\\(?P<name>[A-Za-z]+)\[[^\]\r\n]*\]"),
        r"\\{name}\[[^]\r\n]*\]",
    ),
    (
        "backslash_angle",
        re.compile(r"\\(?P<name>[A-Za-z]+)<[^>\r\n]*>"),
        r"\\{name}<[^>\r\n]*>",
    ),
    ("mustache", re.compile(r"\{\{[^{}\r\n]+\}\}"), r"\{\{[^{}\r\n]+\}\}"),
    ("template", re.compile(r"\$\{[^{}\r\n]+\}"), r"\$\{[^{}\r\n]+\}"),
    ("percent", re.compile(r"%[A-Za-z_][A-Za-z0-9_]*%"), r"%[A-Za-z_][A-Za-z0-9_]*%"),
    (
        "percent_number",
        re.compile(r"%[0-9]+"),
        r"%[0-9]+",
    ),
    ("angle_tag", re.compile(r"</?[A-Za-z][^>\r\n]*>"), r"</?[A-Za-z][^>\r\n]*>"),
)
_CROSS_LINE_FORMS: tuple[tuple[str, re.Pattern[str], str], ...] = (
    ("backslash_bracket", re.compile(r"\\[A-Za-z]+\["), "]"),
    ("backslash_angle", re.compile(r"\\[A-Za-z]+<"), ">"),
    ("mustache", re.compile(r"\{\{"), "}}"),
    ("template", re.compile(r"\$\{"), "}"),
    ("angle_tag", re.compile(r"</?[A-Za-z]"), ">"),
)
_SCOPES = {
    "database_entry",
    "system",
    "map",
    "event_dialogue",
    "event_choices",
    "event_scrolling_text",
    "event_command",
    "plugin_parameter",
}
_NAMED_CAPTURE = re.compile(r"\(\?<([A-Za-z_][A-Za-z0-9_]*)>")


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(
        description="统计 Builtin 控制符、插件 token、跨行和重叠风险；只从 Agent 审核文件写规则。"
    )
    parser.add_argument("--manual", type=Path, required=True, help="最终 Extract 后的 Manual export TOML")
    parser.add_argument("--output", type=Path, required=True, help="Placeholder 候选 JSON")
    parser.add_argument("--decisions", type=Path, help='Agent 审核 JSON：{"rules": [...]}')
    parser.add_argument("--rules-output", type=Path, help="审核后的 Placeholder TOML")
    parser.add_argument("--replace", action="store_true")
    return parser


def _candidate_pattern(kind: str, match: re.Match[str], template: str) -> str:
    if kind.startswith("backslash_"):
        return template.format(name=re.escape(match.group("name")))
    return template


def _load_rules(path: Path) -> list[dict[str, JsonValue]]:
    root = read_json_object(path, "Placeholder 审核文件")
    validate_object_keys(root, str(path), {"rules"})
    raw_rules = require_list(root.get("rules"), str(path), "rules")
    rules: list[dict[str, JsonValue]] = []
    seen: set[str] = set()
    for number, raw in enumerate(raw_rules, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"第 {number} 条 rule 不是 object", "把每条 rule 写成 JSON object")
        rule = raw
        validate_object_keys(rule, f"{path}:第 {number} 条 rule", {"pattern", "scopes"})
        pattern = rule.get("pattern")
        if not isinstance(pattern, str) or not pattern:
            fail(str(path), f"第 {number} 条 rule 缺少非空 pattern", "填写 PCRE2 pattern")
        captures = _NAMED_CAPTURE.findall(pattern)
        if captures not in ([], ["text"]):
            fail(str(path), f"第 {number} 条 rule 的命名捕获无效", "不用命名捕获，或只用一个 (?<text>...)")
        if "scopes" in rule:
            raw_scopes = require_list(rule["scopes"], str(path), f"第 {number} 条 scopes")
            scopes = strings_only(raw_scopes, str(path), f"第 {number} 条 scopes")
            if not scopes or len(scopes) != len(set(scopes)) or not set(scopes) <= _SCOPES:
                fail(
                    str(path),
                    f"第 {number} 条 scopes 为空、重复或包含未知值",
                    "只使用 RPG Maker Rules 规格列出的八个精确 scope",
                )
        identity = json.dumps(rule, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        if identity in seen:
            fail(str(path), f"第 {number} 条 rule 与前面的规则完全重复", "删除重复规则")
        seen.add(identity)
        rules.append(rule)
    return rules


def _rules_toml(rules: list[dict[str, JsonValue]]) -> str:
    if not rules:
        return "rule = []\n"
    chunks: list[str] = []
    for rule in rules:
        lines = ["[[rule]]"]
        scopes = rule.get("scopes")
        if isinstance(scopes, list):
            rendered = ", ".join(toml_string(cast(str, scope)) for scope in scopes)
            lines.append(f"scopes = [{rendered}]")
        lines.append(f"pattern = {toml_string(cast(str, rule['pattern']))}")
        chunks.append("\n".join(lines))
    return "\n\n".join(chunks) + "\n"


def _cross_line_risks(readable_id: str, text: str) -> list[JsonValue]:
    """记录 opener 与 closer 跨 Manual 行的词法事实，不推导 PCRE2。"""
    risks: list[JsonValue] = []
    for kind, opener, closer in _CROSS_LINE_FORMS:
        for match in opener.finditer(text):
            close_at = text.find(closer, match.end())
            if close_at < 0 or "\n" not in text[match.end() : close_at]:
                continue
            start_line = text.count("\n", 0, match.start()) + 1
            end_line = text.count("\n", 0, close_at) + 1
            risks.append(
                {
                    "id": readable_id,
                    "kind": kind,
                    "opening": match.group(0),
                    "closing": closer,
                    "start_line": start_line,
                    "end_line": end_line,
                    "reason": "opener_and_closer_cross_manual_line",
                    "do_not_generate_pattern_without_att_check": True,
                }
            )
    return risks


def _analyze(args: argparse.Namespace) -> int:
    decisions_path = cast(Path | None, args.decisions)
    rules_output = cast(Path | None, args.rules_output)
    if (decisions_path is None) != (rules_output is None):
        fail("命令行参数", "--decisions 与 --rules-output 必须同时提供", "同时提供审核 JSON 和规则输出路径")
    rules = _load_rules(decisions_path) if decisions_path is not None else None
    entries = read_manual(args.manual)
    targets = [args.output] if rules_output is None else [args.output, rules_output]
    inputs = [args.manual] if decisions_path is None else [args.manual, decisions_path]
    protect_outputs(targets, inputs=inputs, replace=args.replace)
    grouped: dict[tuple[str, str], dict[str, JsonValue]] = {}
    control_like: dict[str, dict[str, JsonValue]] = {}
    control_like_occurrences = 0
    overlap_risks: list[JsonValue] = []
    cross_line_risks: list[JsonValue] = []
    multiline_entries = 0
    for entry in entries:
        text = "\n".join(entry.source)
        if len(entry.source) > 1:
            multiline_entries += 1
            cross_line_risks.extend(_cross_line_risks(entry.readable_id, text))
        control_spans: list[tuple[int, int, str]] = []
        for match in _CONTROL_LIKE.finditer(text):
            form = match.group(0)
            control_spans.append((match.start(), match.end(), form))
            control_like_occurrences += 1
            fact = control_like.setdefault(
                form,
                {"observed_form": form, "occurrences": 0, "locations": []},
            )
            fact["occurrences"] = cast(int, fact["occurrences"]) + 1
            cast(list[JsonValue], fact["locations"]).append(entry.readable_id)
        custom_spans: list[tuple[int, int, str]] = []
        for kind, expression, template in _CUSTOM_FORMS:
            for match in expression.finditer(text):
                pattern = _candidate_pattern(kind, match, template)
                key = (kind, pattern)
                overlaps_control = any(
                    match.start() < end and start < match.end() for start, end, _ in control_spans
                )
                candidate = grouped.setdefault(
                    key,
                    {
                        "kind": kind,
                        "observed_form": match.group(0),
                        "suggested_pattern": pattern,
                        "occurrences": 0,
                        "locations": [],
                        "possible_builtin_overlap": False,
                        "do_not_select_without_att_check": False,
                        "semantics": "unconfirmed",
                    },
                )
                candidate["occurrences"] = cast(int, candidate["occurrences"]) + 1
                locations = cast(list[JsonValue], candidate["locations"])
                locations.append(entry.readable_id)
                if overlaps_control:
                    candidate["possible_builtin_overlap"] = True
                    candidate["do_not_select_without_att_check"] = True
                    overlap_risks.append(
                        {
                            "id": entry.readable_id,
                            "forms": [kind, "rpg_control_like"],
                            "observed_form": match.group(0),
                            "reason": "possible_builtin_overlap",
                        }
                    )
                custom_spans.append((match.start(), match.end(), kind))
        custom_spans.sort()
        for index, left in enumerate(custom_spans):
            for right in custom_spans[index + 1 :]:
                if right[0] >= left[1]:
                    break
                overlap_risks.append(
                    {
                        "id": entry.readable_id,
                        "forms": sorted({left[2], right[2]}),
                        "reason": "候选保护范围相交，同时选用会被 ATT 拒绝",
                    }
                )
    candidates = [grouped[key] for key in sorted(grouped)]
    result: dict[str, JsonValue] = {
        "manual": str(args.manual.resolve()),
        "translation_entries": len(entries),
        "control_like_occurrences": control_like_occurrences,
        "control_like_forms": [control_like[key] for key in sorted(control_like)],
        "multiline_entries": multiline_entries,
        "cross_line_risks": cross_line_risks,
        "custom_candidates": candidates,
        "overlap_risks": overlap_risks,
        "decision_required": True,
        "notes": [
            "control_like 只是保守词法外形，不是 ATT Builtin/PCRE2 的复制实现或权威命中。",
            "候选外形不证明语义；Agent 必须核对实际插件协议、scope、NaturalText 与 wrapper。",
            "cross_line_risks 只说明 opener 与 closer 横跨 Manual 行，不自动生成跨行 PCRE2。",
            "Python 不复制 PCRE2；最终文件由 ATT 的生产解析和 Translate 规划验收。",
        ],
    }
    write_json_with_optional_text(
        args.output,
        result,
        text_path=rules_output,
        text=_rules_toml(rules) if rules is not None else None,
        replace=args.replace,
    )
    print(
        f"已检查 {len(entries)} 个当前可翻译条目；RPG control-like 外形 {control_like_occurrences} 处，"
        f"自定义外形候选 {len(candidates)} 类。"
    )
    print(f"详细候选：{display_path(args.output)}")
    if rules is not None and rules_output is not None:
        print(f"已按 Agent 审核写入 {len(rules)} 条 Placeholder Rules：{display_path(rules_output)}")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _analyze(parsed))
