#!/usr/bin/env python3
"""用 ATT ownership 导出审计每个 RPG Maker 文本来源的唯一所有者。"""

from __future__ import annotations

import argparse
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    ToolError,
    display_path,
    fail,
    parse_json_text,
    protect_outputs,
    read_json_object,
    require_file,
    require_list,
    require_string,
    run_cli,
    validate_object_keys,
    write_json,
)

_OWNERS = {"builtin", "rules", "generic", "excluded", "unresolved"}
_RULE_FIELDS = {"file", "plugin", "code", "parameter", "path", "decode_json", "pattern"}
_GENERIC_EVIDENCE = {
    "exact_location",
    "active_runtime_consumer",
    "player_visible_non_image_text",
    "builtin_not_owner",
    "rules_cannot_map_reversibly",
    "extract_group_unit_write_back_mapping",
    "unique_owner",
}


@dataclass(frozen=True, slots=True)
class OwnershipEntry:
    manual_id: str
    owner: str
    rule_number: int | None


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(
        description="对照 ATT ownership 导出、当前 Rules 和 manifest 检查每个 inventory 来源。"
    )
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument(
        "--ownership", type=Path, required=True, help="manual export --ownership 生成的 JSONL"
    )
    parser.add_argument("--rules", type=Path, required=True, help="本次 Extract 使用的 Rules TOML")
    parser.add_argument("--rules-manifest", type=Path, required=True)
    parser.add_argument("--decisions", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replace", action="store_true")
    return parser


def _inventory_sources(path: Path) -> dict[str, dict[str, JsonValue]]:
    root = read_json_object(path, "RPG Maker inventory")
    raw_sources = require_list(root.get("text_sources"), str(path), "text_sources")
    sources: dict[str, dict[str, JsonValue]] = {}
    for number, raw in enumerate(raw_sources, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"第 {number} 个 text_source 不是 object", "重新运行 inspect_rpg_maker.py")
        source = require_string(raw.get("source"), str(path), f"text_sources[{number}].source")
        if source in sources:
            fail(str(path), f"inventory 重复列出来源 {source}", "重新运行 inspect_rpg_maker.py")
        sources[source] = raw
    return sources


def _generic_evidence(value: JsonValue, object_name: str) -> dict[str, JsonValue]:
    if not isinstance(value, dict):
        fail(object_name, "Generic 决定缺少 evidence object", "补全 Generic 七项直接证据")
    validate_object_keys(value, object_name, _GENERIC_EVIDENCE)
    missing = sorted(_GENERIC_EVIDENCE - set(value))
    if missing:
        fail(object_name, f"Generic 证据缺少：{', '.join(missing)}", "实际核对缺少项，不能用推测补齐")
    for field in sorted(_GENERIC_EVIDENCE):
        fact = require_string(value[field], object_name, field)
        if not fact.strip():
            fail(object_name, f"Generic 证据 {field} 为空白", "填写该具体来源已经核对的直接事实")
    return value


def _decisions(path: Path, sources: dict[str, dict[str, JsonValue]]) -> dict[str, dict[str, JsonValue]]:
    root = read_json_object(path, "文本所有者审核文件")
    validate_object_keys(root, str(path), {"sources"})
    decisions: dict[str, dict[str, JsonValue]] = {}
    for number, raw in enumerate(require_list(root.get("sources"), str(path), "sources"), start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"第 {number} 个 source 决定不是 object", "把决定写成 JSON object")
        validate_object_keys(raw, f"{path}:sources[{number}]", {"source", "owner", "evidence"})
        source = require_string(raw.get("source"), str(path), f"sources[{number}].source")
        owner = require_string(raw.get("owner"), str(path), f"sources[{number}].owner")
        if source not in sources:
            fail(str(path), f"决定引用了 inventory 中不存在的来源 {source}", "使用 inventory 的精确 source")
        if source in decisions:
            fail(str(path), f"来源 {source} 被分配了多次", "一个来源只保留一项决定")
        if owner not in _OWNERS:
            fail(
                str(path),
                f"来源 {source} 的 owner {owner} 无效",
                "使用 builtin、rules、generic、excluded 或 unresolved",
            )
        is_builtin = sources[source].get("builtin") is True
        if (owner == "builtin") != is_builtin:
            fail(
                str(path),
                f"来源 {source} 的 builtin owner 与 inventory 不一致",
                "Builtin 来源只能保持 builtin；其他来源不能分配给 builtin",
            )
        if owner == "generic":
            raw["evidence"] = _generic_evidence(raw.get("evidence"), f"{path}:{source}")
        elif owner in {"builtin", "rules", "excluded"}:
            evidence = require_string(raw.get("evidence"), f"{path}:{source}", "evidence")
            if not evidence.strip():
                fail(f"{path}:{source}", "evidence 为空白", "写明这个具体来源的调查事实")
        decisions[source] = raw
    return decisions


def _rule_source(rule: dict[str, JsonValue], object_name: str) -> str:
    file_name = rule.get("file")
    if isinstance(file_name, str):
        return f"data/{file_name}"
    plugin_name = rule.get("plugin")
    if isinstance(plugin_name, str):
        return f"plugin:{plugin_name}:parameters"
    code = rule.get("code")
    parameter = rule.get("parameter")
    if (
        isinstance(code, int)
        and not isinstance(code, bool)
        and isinstance(parameter, int)
        and not isinstance(parameter, bool)
    ):
        return f"event-command:{code}:parameter:{parameter}"
    fail(object_name, "规则没有恰好一种可识别来源", "使用 file、plugin 或 code+parameter")


def _normalize_rule(raw: object, object_name: str) -> dict[str, JsonValue]:
    if not isinstance(raw, dict):
        fail(object_name, "规则不是 table/object", "重新生成当前 Rules TOML 与 manifest")
    rule = cast(dict[str, object], raw)
    unknown = sorted(set(rule) - _RULE_FIELDS)
    if unknown:
        fail(object_name, f"规则存在未知字段：{', '.join(unknown)}", "使用当前 Rules 规格字段")
    normalized: dict[str, JsonValue] = {}
    for field, value in rule.items():
        if isinstance(value, (str, bool, int)):
            normalized[field] = value
        else:
            fail(object_name, f"规则字段 {field} 类型无效", "重新生成当前 Rules TOML 与 manifest")
    source_count = (
        int("file" in normalized)
        + int("plugin" in normalized)
        + int("code" in normalized or "parameter" in normalized)
    )
    if source_count != 1 or ("code" in normalized) != ("parameter" in normalized):
        fail(object_name, "规则没有恰好选择一种来源", "使用 file、plugin 或 code+parameter")
    for field in ("file", "plugin", "path", "pattern"):
        if field in normalized and (not isinstance(normalized[field], str) or not normalized[field]):
            fail(object_name, f"规则字段 {field} 不是非空 string", "重新生成当前 Rules TOML")
    for field in ("code", "parameter"):
        if field in normalized and (
            not isinstance(normalized[field], int)
            or isinstance(normalized[field], bool)
            or cast(int, normalized[field]) < 0
        ):
            fail(object_name, f"规则字段 {field} 不是非负整数", "重新生成当前 Rules TOML")
    if "decode_json" in normalized and not isinstance(normalized["decode_json"], bool):
        fail(object_name, "规则字段 decode_json 不是 boolean", "重新生成当前 Rules TOML")
    return normalized


def _rules_toml(path: Path) -> list[dict[str, JsonValue]]:
    source = require_file(path, "Extract Rules TOML")
    try:
        root = cast(dict[str, object], tomllib.loads(source.read_text(encoding="utf-8-sig")))
    except tomllib.TOMLDecodeError as error:
        fail(str(source), f"Extract Rules TOML 语法错误：{error}", "修正或重新生成 Rules TOML")
    if set(root) != {"rule"}:
        fail(str(source), "Rules TOML 根字段不是唯一的 rule", "重新生成当前 Rules TOML")
    raw_rules = root.get("rule")
    if not isinstance(raw_rules, list):
        fail(str(source), "rule 不是 array of tables", "使用 [[rule]] 或明确 rule = []")
    return [
        _normalize_rule(raw, f"{source}:第 {number} 条规则")
        for number, raw in enumerate(cast(list[object], raw_rules), start=1)
    ]


def _manifest(
    path: Path,
    rules: list[dict[str, JsonValue]],
    inventory_sources: dict[str, dict[str, JsonValue]],
) -> dict[int, str]:
    root = read_json_object(path, "Extract Rules manifest")
    validate_object_keys(root, str(path), {"rules"})
    raw_rows = require_list(root.get("rules"), str(path), "rules")
    if len(raw_rows) != len(rules):
        fail(
            str(path),
            f"manifest 有 {len(raw_rows)} 条，但当前 Rules TOML 有 {len(rules)} 条",
            "用 analyze_extract_rules.py 对当前 Rules 重新生成 manifest",
        )
    result: dict[int, str] = {}
    for expected_number, (raw, current_rule) in enumerate(zip(raw_rows, rules, strict=True), start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"manifest 第 {expected_number} 项不是 object", "重新生成 manifest")
        validate_object_keys(
            raw,
            f"{path}:rules[{expected_number}]",
            {"rule_number", "source", "rule"},
        )
        rule_number = raw.get("rule_number")
        if rule_number != expected_number:
            fail(
                str(path),
                f"manifest 第 {expected_number} 项的 rule_number 不是自然序号 {expected_number}",
                "重新生成 manifest，不要手工重排",
            )
        source = require_string(raw.get("source"), str(path), f"rules[{expected_number}].source")
        manifest_rule = _normalize_rule(raw.get("rule"), f"{path}:rules[{expected_number}].rule")
        if manifest_rule != current_rule:
            fail(
                str(path),
                f"manifest 第 {expected_number} 项与当前 Rules TOML 不一致",
                "用 analyze_extract_rules.py 对当前 Rules 重新生成 manifest",
            )
        expected_source = _rule_source(current_rule, f"{path}:rules[{expected_number}]")
        if source != expected_source:
            fail(
                str(path),
                f"manifest 第 {expected_number} 项 source 应为 {expected_source}，实际为 {source}",
                "重新生成 manifest，不要手工改写来源",
            )
        fact = inventory_sources.get(source)
        if fact is None:
            fail(
                str(path),
                f"manifest 来源 {source} 不在 inventory 中",
                "重新生成同一游戏的 inventory 和 manifest",
            )
        if fact.get("rules_supported") is False:
            fail(
                str(path),
                f"manifest 来源 {source} 已被 inventory 标为 Rules 不支持",
                "重新审核该来源的所有者",
            )
        result[expected_number] = source
    return result


def _ownership(path: Path) -> list[OwnershipEntry]:
    source = require_file(path, "ATT ownership JSONL")
    entries: list[OwnershipEntry] = []
    seen: set[str] = set()
    for line_number, text in enumerate(source.read_text(encoding="utf-8-sig").splitlines(), start=1):
        if not text.strip():
            fail(str(source), f"第 {line_number} 行为空", "重新运行 manual export --ownership")
        raw = parse_json_text(text, f"{source}:第 {line_number} 行")
        if not isinstance(raw, dict):
            fail(str(source), f"第 {line_number} 行不是 JSON object", "重新运行 manual export --ownership")
        raw_owner = raw.get("owner")
        allowed = {"manual_id", "owner"} if raw_owner == "builtin" else {"manual_id", "owner", "rule_number"}
        validate_object_keys(raw, f"{source}:第 {line_number} 行", allowed)
        manual_id = require_string(raw.get("manual_id"), str(source), f"第 {line_number} 行 manual_id")
        if manual_id in seen:
            fail(str(source), f"manual_id {manual_id} 重复", "重新运行同一快照的 manual export --ownership")
        seen.add(manual_id)
        owner = require_string(raw_owner, str(source), f"第 {line_number} 行 owner")
        if owner not in {"builtin", "rules"}:
            fail(str(source), f"第 {line_number} 行 owner 不是 builtin 或 rules", "重新运行当前 ATT 导出")
        rule_number: int | None = None
        if owner == "rules":
            raw_number = raw.get("rule_number")
            if not isinstance(raw_number, int) or isinstance(raw_number, bool) or raw_number <= 0:
                fail(str(source), f"第 {line_number} 行 rule_number 不是正整数", "重新运行当前 ATT 导出")
            rule_number = raw_number
        entries.append(OwnershipEntry(manual_id=manual_id, owner=owner, rule_number=rule_number))
    return entries


def _builtin_file(source: str) -> str | None:
    prefix = "data/"
    for suffix in (":builtin-fields", ":builtin-events"):
        if source.startswith(prefix) and source.endswith(suffix):
            return source[len(prefix) : -len(suffix)]
    return None


def _audit(args: argparse.Namespace) -> int:
    protect_outputs(
        [args.output],
        inputs=[args.inventory, args.ownership, args.rules, args.rules_manifest, args.decisions],
        replace=args.replace,
    )
    sources = _inventory_sources(args.inventory)
    decisions = _decisions(args.decisions, sources)
    rules = _rules_toml(args.rules)
    manifest = _manifest(args.rules_manifest, rules, sources)
    ownership = _ownership(args.ownership)

    builtin_by_file: dict[str, str] = {}
    for source, fact in sources.items():
        if fact.get("builtin") is not True:
            continue
        file_name = _builtin_file(source)
        if file_name is None:
            fail(
                str(args.inventory),
                f"Builtin 来源 {source} 不是自然数据来源",
                "重新运行 inspect_rpg_maker.py",
            )
        if file_name in builtin_by_file:
            fail(
                str(args.inventory), f"文件 {file_name} 有多个 Builtin 来源", "重新运行 inspect_rpg_maker.py"
            )
        builtin_by_file[file_name] = source

    ids_by_source: dict[str, list[str]] = {source: [] for source in sources}
    ids_by_rule: dict[int, list[str]] = {number: [] for number in manifest}
    unmapped: list[JsonValue] = []
    for entry in ownership:
        mapped: str | None
        if entry.owner == "builtin":
            file_name, separator, _ = entry.manual_id.partition(":")
            mapped = builtin_by_file.get(file_name) if separator else None
            if mapped is None:
                unmapped.append(
                    {
                        "manual_id": entry.manual_id,
                        "owner": "builtin",
                        "reason": "可读 ID 的来源文件不对应 inventory 中唯一 Builtin 来源",
                    }
                )
                continue
        else:
            rule_number = cast(int, entry.rule_number)
            mapped = manifest.get(rule_number)
            if mapped is None:
                unmapped.append(
                    {
                        "manual_id": entry.manual_id,
                        "owner": "rules",
                        "rule_number": rule_number,
                        "reason": "ownership 的规则自然序号不在当前 manifest 中",
                    }
                )
                continue
            ids_by_rule[rule_number].append(entry.manual_id)
        ids_by_source[mapped].append(entry.manual_id)

    unused_rules = [number for number, ids in ids_by_rule.items() if not ids]
    conflicts: list[JsonValue] = []
    counts = {owner: 0 for owner in sorted(_OWNERS)}
    unresolved: list[str] = []
    rows: list[JsonValue] = []
    manifest_sources = set(manifest.values())
    for source in sorted(sources):
        fact = sources[source]
        decision = decisions.get(source)
        if fact.get("builtin") is True:
            owner = "builtin"
            evidence: JsonValue = (
                decision.get("evidence") if decision is not None else "ATT ownership 导出标记为 Builtin"
            )
        elif decision is None:
            owner = "unresolved"
            evidence = "尚未提供决定"
        else:
            owner = cast(str, decision["owner"])
            evidence = decision.get("evidence")
        counts[owner] += 1
        if owner == "unresolved":
            unresolved.append(source)
        manual_ids = ids_by_source[source]
        if owner in {"generic", "excluded", "unresolved"} and manual_ids:
            conflicts.append(
                {
                    "source": source,
                    "owner": owner,
                    "reason": "当前 MV/MZ ownership 导出仍包含该来源的 Manual 条目",
                    "manual_ids": manual_ids,
                }
            )
        if owner == "rules" and source not in manifest_sources:
            conflicts.append(
                {
                    "source": source,
                    "owner": owner,
                    "reason": "决定分配给 Rules，但当前 manifest 没有映射到该来源的规则",
                    "manual_ids": manual_ids,
                }
            )
        if owner != "rules" and source in manifest_sources:
            conflicts.append(
                {
                    "source": source,
                    "owner": owner,
                    "reason": "当前 manifest 有规则映射到该来源，但审核决定不是 Rules",
                    "manual_ids": manual_ids,
                }
            )
        if owner == "rules" and not manual_ids:
            conflicts.append(
                {
                    "source": source,
                    "owner": owner,
                    "reason": "当前 ownership 导出没有该 Rules 来源的 Manual 条目",
                    "manual_ids": [],
                }
            )
        rows.append(
            {
                "source": source,
                "kind": fact.get("kind", "unknown"),
                "owner": owner,
                "evidence": evidence,
                "manual_entry_count": len(manual_ids),
                "manual_ids": manual_ids,
            }
        )

    complete = not unresolved and not unmapped and not unused_rules and not conflicts
    result: dict[str, JsonValue] = {
        "inventory": str(args.inventory.resolve()),
        "ownership": str(args.ownership.resolve()),
        "rules": str(args.rules.resolve()),
        "rules_manifest": str(args.rules_manifest.resolve()),
        "complete": complete,
        "counts": counts,
        "ownership_entry_count": len(ownership),
        "sources": rows,
        "unresolved_sources": unresolved,
        "unmapped_ownership_entries": unmapped,
        "unused_rule_numbers": unused_rules,
        "ownership_conflicts": conflicts,
    }
    write_json(args.output, result, replace=args.replace)
    print(
        f"所有者审计：Builtin {counts['builtin']}，Rules {counts['rules']}，Generic {counts['generic']}，"
        f"排除 {counts['excluded']}，未确认 {counts['unresolved']}。"
    )
    print(f"审计结果：{display_path(args.output)}")
    problem_count = len(unresolved) + len(unmapped) + len(unused_rules) + len(conflicts)
    if problem_count:
        raise ToolError(
            object_name=str(args.output.resolve()),
            reason=f"所有者审计发现 {problem_count} 个未确认映射或冲突",
            impact="审计报告已写入，但不能声称文本覆盖完整；游戏原文件没有修改",
            help_text="按报告修正 inventory、当前 Rules/manifest、ownership 快照或审核决定后重试",
        )
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _audit(parsed))
