#!/usr/bin/env python3
"""审计每个文本来源的唯一 Builtin、Rules、Generic 或排除决定。"""

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
    ToolError,
    display_path,
    fail,
    protect_outputs,
    read_json_object,
    read_manual,
    require_list,
    require_string,
    run_cli,
    validate_object_keys,
    write_json,
)
from att_toolbox.rpg import BUILTIN_DATABASE_FIELDS

_OWNERS = {"builtin", "rules", "generic", "excluded", "unresolved"}
_GENERIC_EVIDENCE = {
    "exact_location",
    "active_runtime_consumer",
    "player_visible_non_image_text",
    "builtin_not_owner",
    "rules_cannot_map_reversibly",
    "extract_group_unit_write_back_mapping",
    "unique_owner",
}


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="检查 inventory 中的每个文本来源是否有且只有一个已证实所有者。")
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--manual", type=Path, required=True, help="最终 Extract 的 Manual export TOML")
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
        fact = raw
        source = require_string(fact.get("source"), str(path), f"text_sources[{number}].source")
        if source in sources:
            fail(str(path), f"inventory 重复列出来源 {source}", "重新运行当前版本的 inspect_rpg_maker.py")
        sources[source] = fact
    return sources


def _generic_evidence(value: JsonValue, object_name: str) -> dict[str, JsonValue]:
    if not isinstance(value, dict):
        fail(object_name, "Generic 决定缺少 evidence object", "补全 Generic 七项直接证据")
    evidence = value
    validate_object_keys(evidence, object_name, _GENERIC_EVIDENCE)
    missing = sorted(_GENERIC_EVIDENCE - set(evidence))
    if missing:
        fail(object_name, f"Generic 证据缺少：{', '.join(missing)}", "实际核对缺少项，不能用推测补齐")
    for field in sorted(_GENERIC_EVIDENCE):
        fact = require_string(evidence[field], object_name, field)
        if not fact.strip():
            fail(object_name, f"Generic 证据 {field} 为空白", "填写该具体来源已经人工核对的直接事实")
    return evidence


def _decisions(path: Path, known_sources: set[str]) -> dict[str, dict[str, JsonValue]]:
    root = read_json_object(path, "文本所有者审核文件")
    validate_object_keys(root, str(path), {"sources"})
    raw_decisions = require_list(root.get("sources"), str(path), "sources")
    decisions: dict[str, dict[str, JsonValue]] = {}
    for number, raw in enumerate(raw_decisions, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"第 {number} 个 source 决定不是 object", "把决定写成 JSON object")
        decision = raw
        validate_object_keys(
            decision,
            f"{path}:sources[{number}]",
            {"source", "owner", "evidence", "manual_prefixes", "zero_text_reason"},
        )
        source = require_string(decision.get("source"), str(path), f"sources[{number}].source")
        owner = require_string(decision.get("owner"), str(path), f"sources[{number}].owner")
        if source not in known_sources:
            fail(
                str(path),
                f"决定引用了 inventory 中不存在的来源 {source}",
                "使用 inventory 中的精确 source 值",
            )
        if source in decisions:
            fail(str(path), f"来源 {source} 被分配了多次", "一个来源只保留一项决定")
        if owner not in _OWNERS:
            fail(
                str(path),
                f"来源 {source} 的 owner {owner} 无效",
                "使用 builtin、rules、generic、excluded 或 unresolved",
            )
        inventory_builtin = source.endswith((":builtin-fields", ":builtin-events"))
        if (owner == "builtin") != inventory_builtin:
            fail(
                str(path),
                f"来源 {source} 的 builtin owner 与 inventory 不一致",
                "Builtin 来源只能保持 builtin；其他来源选择 rules、generic、excluded 或 unresolved",
            )
        if owner == "generic":
            decision["evidence"] = _generic_evidence(decision.get("evidence"), f"{path}:{source}")
        elif owner in {"builtin", "rules", "excluded"}:
            evidence = require_string(decision.get("evidence"), f"{path}:{source}", "evidence")
            if not evidence.strip():
                fail(f"{path}:{source}", "evidence 为空白", "写明这个具体来源的调查事实")
        prefixes = decision.get("manual_prefixes", [])
        if not isinstance(prefixes, list):
            fail(f"{path}:{source}", "manual_prefixes 不是 array", "使用公开可读 ID 前缀的 string array")
        checked_prefixes: list[JsonValue] = []
        for prefix_number, prefix in enumerate(prefixes, start=1):
            if not isinstance(prefix, str) or not prefix.strip() or prefix.strip() != prefix:
                fail(
                    f"{path}:{source}",
                    f"manual_prefixes 第 {prefix_number} 项为空或含首尾空白",
                    "填写最终 Manual 中逐字可核对的公开自然前缀",
                )
            checked_prefixes.append(prefix)
        if len(set(cast(list[str], checked_prefixes))) != len(checked_prefixes):
            fail(f"{path}:{source}", "manual_prefixes 存在重复项", "删除重复前缀")
        decision["manual_prefixes"] = checked_prefixes
        zero_text_reason = decision.get("zero_text_reason")
        if zero_text_reason is not None and (
            not isinstance(zero_text_reason, str)
            or not zero_text_reason.strip()
            or zero_text_reason.strip() != zero_text_reason
        ):
            fail(f"{path}:{source}", "zero_text_reason 为空或含首尾空白", "填写 Rules 合法零文本的直接证据")
        decisions[source] = decision
    return decisions


_DATA_SOURCE = re.compile(r"\Adata/(?P<file>[^/:]+\.json)(?::builtin-(?:fields|events))?\Z")


def _automatic_match(source: str, readable_id: str, files_with_builtin_peer: set[str]) -> bool:
    match = _DATA_SOURCE.fullmatch(source)
    if match is not None:
        file_name = match.group("file")
        is_builtin = ":builtin-" in source
        has_builtin_peer = file_name in files_with_builtin_peer
        if not readable_id.startswith(f"{file_name}:"):
            return False
        if not is_builtin:
            return not has_builtin_peer
        remainder = readable_id[len(file_name) + 1 :]
        fields = BUILTIN_DATABASE_FIELDS.get(file_name)
        if fields is not None:
            field_pattern = "|".join(re.escape(field) for field in fields)
            return re.fullmatch(rf"[0-9]+:(?:{field_pattern})", remainder) is not None
        if file_name == "System.json":
            return (
                re.fullmatch(
                    r"(?:gameTitle|currencyUnit|elements:[0-9]+|skillTypes:[0-9]+|"
                    r"weaponTypes:[0-9]+|armorTypes:[0-9]+|equipTypes:[0-9]+|"
                    r"terms:(?:basic|commands|params):[0-9]+|terms:messages:.+)",
                    remainder,
                )
                is not None
            )
        if file_name.startswith("Map"):
            return (
                remainder == "displayName"
                or re.fullmatch(
                    r"event[0-9]+:page[1-9][0-9]*:(?:dialogue|choices|scrolling)[1-9][0-9]*(?::speaker)?",
                    remainder,
                )
                is not None
            )
        return False
    return False


class _PrefixNode:
    __slots__ = ("children", "sources")

    def __init__(self) -> None:
        self.children: dict[str, _PrefixNode] = {}
        self.sources: set[str] = set()


class _ManualPrefixIndex:
    """按字符索引人工前缀，避免每个 Manual 位置扫描全部来源和前缀。"""

    def __init__(self, decisions: dict[str, dict[str, JsonValue]]) -> None:
        self._root = _PrefixNode()
        for source, decision in decisions.items():
            raw_prefixes = decision.get("manual_prefixes")
            if not isinstance(raw_prefixes, list):
                continue
            for prefix in cast(list[str], raw_prefixes):
                node = self._root
                for character in prefix:
                    node = node.children.setdefault(character, _PrefixNode())
                node.sources.add(source)

    def matches(self, readable_id: str) -> set[str]:
        matches: set[str] = set()
        node = self._root
        for character in readable_id:
            child = node.children.get(character)
            if child is None:
                break
            node = child
            matches.update(node.sources)
        return matches


def _automatic_source_index(sources: set[str]) -> tuple[dict[str, list[str]], set[str]]:
    by_file: dict[str, list[str]] = {}
    files_with_builtin_peer: set[str] = set()
    for source in sorted(sources):
        match = _DATA_SOURCE.fullmatch(source)
        if match is None:
            continue
        file_name = match.group("file")
        by_file.setdefault(file_name, []).append(source)
        if ":builtin-" in source:
            files_with_builtin_peer.add(file_name)
    return by_file, files_with_builtin_peer


def _audit(args: argparse.Namespace) -> int:
    protect_outputs(
        [args.output],
        inputs=[args.inventory, args.decisions, args.manual],
        replace=args.replace,
    )
    sources = _inventory_sources(args.inventory)
    decisions = _decisions(args.decisions, set(sources))
    manual_entries = read_manual(args.manual)
    locations_by_source: dict[str, list[str]] = {source: [] for source in sources}
    unresolved_mapping: list[str] = []
    duplicate_locations: list[JsonValue] = []
    all_sources = set(sources)
    automatic_sources, files_with_builtin_peer = _automatic_source_index(all_sources)
    prefix_index = _ManualPrefixIndex(decisions)
    for entry in manual_entries:
        claim_set = prefix_index.matches(entry.readable_id)
        file_name, separator, _ = entry.readable_id.partition(":")
        if separator:
            for source in automatic_sources.get(file_name, []):
                if _automatic_match(source, entry.readable_id, files_with_builtin_peer):
                    claim_set.add(source)
        claims = sorted(claim_set)
        if not claims:
            unresolved_mapping.append(entry.readable_id)
            continue
        if len(claims) > 1:
            duplicate_locations.append({"location": entry.readable_id, "claimed_by": claims})
            continue
        locations_by_source[claims[0]].append(entry.readable_id)
    rows: list[JsonValue] = []
    unresolved: list[str] = []
    ownership_conflicts: list[JsonValue] = []
    counts = {"builtin": 0, "rules": 0, "generic": 0, "excluded": 0, "unresolved": 0}
    for source in sorted(sources):
        fact = sources[source]
        builtin = fact.get("builtin") is True
        if builtin:
            decision = decisions.get(source)
            if decision is not None and decision.get("owner") != "builtin":
                fail(
                    str(args.decisions),
                    f"Builtin 来源 {source} 不应再分配给其他 owner",
                    "删除该决定；Builtin 之外的字段应在 inventory 中使用独立 source",
                )
            owner = "builtin"
            evidence: JsonValue = (
                decision.get("evidence")
                if decision is not None
                else "RPG Maker Extract 规格的精确 Builtin 覆盖"
            )
        else:
            decision = decisions.get(source)
            if decision is None:
                owner = "unresolved"
                evidence = "尚未提供决定"
            else:
                owner = cast(str, decision["owner"])
                evidence = decision.get("evidence")
        counts[owner] += 1
        if owner == "unresolved":
            unresolved.append(source)
        extracted_locations = locations_by_source[source]
        if owner in {"generic", "excluded", "unresolved"} and extracted_locations:
            ownership_conflicts.append(
                {
                    "source": source,
                    "owner": owner,
                    "reason": "owner 不应由 RPG Maker Extract 拥有，但最终 Manual 中存在该来源位置",
                    "locations": extracted_locations,
                }
            )
        if owner == "rules" and fact.get("rules_supported") is False:
            ownership_conflicts.append(
                {
                    "source": source,
                    "owner": owner,
                    "reason": "inventory 已确认该来源不能由 Extract Rules 的 file/plugin/command 表达",
                    "locations": extracted_locations,
                }
            )
        if owner == "rules" and not extracted_locations:
            decision = decisions.get(source)
            zero_reason = decision.get("zero_text_reason") if decision is not None else None
            if not isinstance(zero_reason, str):
                ownership_conflicts.append(
                    {
                        "source": source,
                        "owner": owner,
                        "reason": "Rules 来源没有出现在最终 Manual，且未提供 zero_text_reason",
                        "locations": [],
                    }
                )
        rows.append(
            {
                "source": source,
                "kind": fact.get("kind", "unknown"),
                "owner": owner,
                "evidence": evidence,
                "extracted_entry_count": len(extracted_locations),
                "extracted_locations": extracted_locations,
            }
        )
    complete = (
        not unresolved and not unresolved_mapping and not duplicate_locations and not ownership_conflicts
    )
    result: dict[str, JsonValue] = {
        "inventory": str(args.inventory.resolve()),
        "manual": str(args.manual.resolve()),
        "complete": complete,
        "counts": counts,
        "sources": rows,
        "unresolved_sources": unresolved,
        "unresolved_mapping": unresolved_mapping,
        "duplicate_location_ownership": duplicate_locations,
        "ownership_conflicts": ownership_conflicts,
    }
    write_json(args.output, result, replace=args.replace)
    print(
        f"所有者审计：Builtin {counts['builtin']}，Rules {counts['rules']}，Generic {counts['generic']}，"
        f"排除 {counts['excluded']}，未确认 {counts['unresolved']}。"
    )
    print(f"审计结果：{display_path(args.output)}")
    problem_count = (
        len(unresolved) + len(unresolved_mapping) + len(duplicate_locations) + len(ownership_conflicts)
    )
    if problem_count:
        raise ToolError(
            object_name=str(args.output.resolve()),
            reason=f"所有者审计发现 {problem_count} 个未确认映射或冲突",
            impact="审计报告已写入，但不能声称文本覆盖完整；游戏原文件没有修改",
            help_text="逐项处理 unresolved_sources、unresolved_mapping、重复位置和所有者冲突后重试",
        )
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _audit(parsed))
