#!/usr/bin/env python3
"""把 Agent 审核的术语与译名写成严格 ATT TOML。"""

from __future__ import annotations

import argparse
import sys
import unicodedata
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    atomic_write_text,
    display_path,
    fail,
    protect_outputs,
    read_json_object,
    run_cli,
    toml_string,
    validate_object_keys,
)


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="验证术语、译名和 trigger 唯一性，再写 terminology.toml。")
    parser.add_argument("--input", type=Path, required=True, help='Agent 审核 JSON：{"terms": [...]}')
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replace", action="store_true")
    return parser


def _text(value: JsonValue, object_name: str, field: str, *, allow_line_feed: bool = False) -> str:
    if not isinstance(value, str) or not value.strip():
        fail(object_name, f"{field} 必须是非空白 string", f"填写非空白 {field}")
    if value.strip() != value:
        fail(object_name, f"{field} 含首尾空白", f"删除 {field} 的首尾空白；原值不会自动 trim")
    if any(
        unicodedata.category(character) in {"Cc", "Cf", "Cs", "Zl", "Zp"}
        and not (allow_line_feed and character == "\n")
        for character in value
    ):
        allowed = "；trigger 只允许内部 LF 换行" if allow_line_feed else ""
        fail(object_name, f"{field} 包含 Unicode 控制字符{allowed}", f"删除 {field} 中的控制字符")
    return value


def _terms(path: Path) -> list[dict[str, JsonValue]]:
    root = read_json_object(path, "Agent 术语审核 JSON")
    validate_object_keys(root, str(path), {"terms"})
    raw_terms = root.get("terms")
    if not isinstance(raw_terms, list):
        fail(str(path), "terms 必须是 array", '使用 {"terms": [...]} 根结构')
    result: list[dict[str, JsonValue]] = []
    seen_terms: set[str] = set()
    seen_triggers: set[str] = set()
    for number, raw in enumerate(raw_terms, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"第 {number} 项 term 不是 object", "把每项写成 JSON object")
        item = raw
        validate_object_keys(item, f"{path}:terms[{number}]", {"term", "translation", "triggers"})
        term = _text(item.get("term"), str(path), f"terms[{number}].term")
        translation = _text(item.get("translation"), str(path), f"terms[{number}].translation")
        if term in seen_terms:
            fail(str(path), f"重复术语：{term}", "每个 term 只保留一项")
        seen_terms.add(term)
        triggers: list[str] | None = None
        if "triggers" in item:
            raw_triggers = item["triggers"]
            if not isinstance(raw_triggers, list) or not raw_triggers:
                fail(
                    str(path),
                    f"{term} 的显式 triggers 不是非空 array",
                    "删除 triggers 以使用默认 [term]，或填写非空数组",
                )
            triggers = []
            for trigger_number, raw_trigger in enumerate(raw_triggers, start=1):
                trigger = _text(
                    raw_trigger,
                    str(path),
                    f"{term}.triggers[{trigger_number}]",
                    allow_line_feed=True,
                )
                triggers.append(trigger)
        effective = triggers if triggers is not None else [term]
        if len(effective) != len(set(effective)):
            fail(str(path), f"{term} 的 triggers 内部重复", "删除重复 trigger")
        conflicts = sorted(set(effective) & seen_triggers)
        if conflicts:
            fail(
                str(path),
                f"trigger 已由其他术语使用：{', '.join(conflicts)}",
                "一个 trigger 只能激活一项全局术语",
            )
        seen_triggers.update(effective)
        normalized: dict[str, JsonValue] = {"term": term, "translation": translation}
        if triggers is not None:
            normalized["triggers"] = triggers
        result.append(normalized)
    return result


def _toml(terms: list[dict[str, JsonValue]]) -> str:
    if not terms:
        return "term = []\n"
    chunks: list[str] = []
    for item in terms:
        lines = [
            "[[term]]",
            f"term = {toml_string(cast(str, item['term']))}",
            f"translation = {toml_string(cast(str, item['translation']))}",
        ]
        triggers = item.get("triggers")
        if isinstance(triggers, list):
            values = ", ".join(toml_string(cast(str, trigger)) for trigger in triggers)
            lines.append(f"triggers = [{values}]")
        chunks.append("\n".join(lines))
    return "\n\n".join(chunks) + "\n"


def _write(args: argparse.Namespace) -> int:
    terms = _terms(args.input)
    protect_outputs([args.output], inputs=[args.input], replace=args.replace)
    atomic_write_text(args.output, _toml(terms), replace=args.replace)
    print(f"已写入 {len(terms)} 项 ATT 术语：{display_path(args.output)}")
    print("下一步：用 att mv|mz|generic translate --terms 交给生产解析、保存与命中验收。")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _write(parsed))
