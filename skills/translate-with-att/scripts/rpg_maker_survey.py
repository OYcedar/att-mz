#!/usr/bin/env python3
"""一次调查 RPG Maker 文本并生成、核对精确所有权计划。"""

from __future__ import annotations

import argparse
import json
import sys
import time
import tomllib
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

# Skill 目录是发行资源，入口进程不得把解释器缓存写回包内。
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    atomic_write_directory,
    atomic_write_text,
    display_path,
    fail,
    protect_outputs,
    read_json_object,
    require_directory,
    require_file,
    run_cli,
    toml_string,
    validate_object_keys,
    write_json,
)
from att_toolbox.survey import (
    GENERIC_EVIDENCE_FIELDS,
    json_lines,
    load_survey,
    read_jsonl,
    scan_game,
    verify_source_baseline,
)
from att_toolbox.survey_projection import project_builtin_units, project_rule_units

_OWNERS = {"rules", "generic", "exclude", "unresolved"}


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="一次扫描 RPG Maker 游戏并生成精确文本所有权计划。")
    subparsers = parser.add_subparsers(dest="command", required=True)

    scan = subparsers.add_parser("scan", help="一次扫描并建立 survey 作业目录")
    scan.add_argument("--game", type=Path, required=True, help="完整游戏安装根")
    scan.add_argument("--output", type=Path, required=True, help="survey 作业目录")
    scan.add_argument("--replace", action="store_true", help="替换已存在的作业目录")

    finalize = subparsers.add_parser("finalize", help="消费审核决定并生成规则、manifest 与覆盖计划")
    finalize.add_argument("--survey", type=Path, required=True, help="scan 生成的 survey 作业目录")
    finalize.add_argument(
        "--decisions",
        type=Path,
        help="逐行 JSON 审核决定；省略时使用 survey/ownership-decisions.jsonl",
    )
    finalize.add_argument("--output", type=Path, required=True, help="计划输出目录")
    finalize.add_argument("--replace", action="store_true", help="替换已存在的计划目录")

    members = subparsers.add_parser("members", help="导出一个关系组的完整位置明细")
    members.add_argument("--survey", type=Path, required=True, help="scan 生成的 survey 作业目录")
    members.add_argument("--group-id", required=True, help="review-groups.jsonl 中的自然 group_id")
    members.add_argument("--output", type=Path, required=True, help="完整成员 JSONL")
    members.add_argument("--replace", action="store_true", help="替换已存在的成员 JSONL")

    audit = subparsers.add_parser("audit", help="用 ATT 所有权导出逐位置核对当前 Extract")
    audit.add_argument("--survey", type=Path, required=True, help="scan 生成的 survey 作业目录")
    audit.add_argument("--plan", type=Path, required=True, help="finalize 生成的计划目录")
    audit.add_argument("--ownership", type=Path, required=True, help="ATT 导出的完整所有权 JSONL")
    audit.add_argument("--output", type=Path, required=True, help="审计 JSON")
    audit.add_argument("--replace", action="store_true", help="替换已存在的审计 JSON")
    return parser


def _json_text(value: JsonValue) -> str:
    return json.dumps(value, ensure_ascii=False, indent=2) + "\n"


def _ownership_decision_template(
    groups: Sequence[Mapping[str, JsonValue]],
) -> list[dict[str, JsonValue]]:
    rows: list[dict[str, JsonValue]] = []
    for group in groups:
        group_id = group.get("group_id")
        if not isinstance(group_id, str) or not group_id:
            fail("review-groups.jsonl", "关系组缺少自然 group_id", "使用当前工具重新执行 scan")
        rows.append({"target": f"group:{group_id}", "owner": "unresolved"})
    return rows


def _scan(args: argparse.Namespace) -> int:
    # scan_game 会自行确认真实内容根；这里先保护显式游戏根，避免输出写进来源树。
    game_root = require_directory(args.game, "游戏目录")
    protect_outputs([args.output], forbidden_roots=[game_root], replace=args.replace)
    bundle = scan_game(game_root)
    locations_text = json_lines(bundle.locations)
    groups_text = json_lines(bundle.review_groups)
    decisions_text = json_lines(_ownership_decision_template(bundle.review_groups))
    metrics_raw = bundle.summary.get("agent_work_metrics")
    metrics = dict(metrics_raw) if isinstance(metrics_raw, dict) else {}
    metrics.update(
        {
            "locations_jsonl_bytes": len(locations_text.encode("utf-8")),
            "review_groups_jsonl_bytes": len(groups_text.encode("utf-8")),
            "ownership_decisions_jsonl_bytes": len(decisions_text.encode("utf-8")),
            "local_commands_required_before_extract": 2,
        }
    )
    survey = dict(bundle.summary)
    survey.pop("agent_work_metrics", None)
    atomic_write_directory(
        args.output,
        {
            "survey.json": _json_text(survey),
            "locations.jsonl": locations_text,
            "review-groups.jsonl": groups_text,
            "ownership-decisions.jsonl": decisions_text,
            "source-baseline.json": _json_text(bundle.source_baseline),
            "agent-work-metrics.json": _json_text(metrics),
        },
        replace=args.replace,
    )
    print(
        f"已完成一次 {str(survey['engine']).upper()} 调查："
        f"位置 {survey['locations']} 个，阅读 packet {survey['review_packets']} 个，"
        f"可决定关系组 {survey['review_groups']} 个。"
    )
    print(f"调查目录：{display_path(args.output)}")
    print("所有权决定模板：ownership-decisions.jsonl（默认 unresolved，只修改已经确认的组）")
    return 0


def _group_members(
    groups: Sequence[Mapping[str, JsonValue]],
    locations: Sequence[Mapping[str, JsonValue]],
) -> dict[str, list[str]]:
    valid_groups: dict[str, list[str]] = {}
    for group in groups:
        group_id = group.get("group_id")
        if not isinstance(group_id, str) or not group_id or group_id in valid_groups:
            fail("review-groups.jsonl", "关系组缺少自然 group_id 或发生重复", "使用当前工具重新执行 scan")
        valid_groups[group_id] = []
    seen_candidates: set[str] = set()
    for location in locations:
        if location.get("classification") != "review":
            continue
        candidate_id = location.get("candidate_id")
        group_id = location.get("review_group_id")
        if (
            not isinstance(candidate_id, str)
            or not candidate_id
            or candidate_id in seen_candidates
            or not isinstance(group_id, str)
            or group_id not in valid_groups
        ):
            fail(
                "locations.jsonl",
                "审核位置缺少唯一 candidate_id 或 review_group_id",
                "使用当前工具重新执行 scan",
            )
        seen_candidates.add(candidate_id)
        valid_groups[group_id].append(candidate_id)
    for group in groups:
        group_id = cast(str, group["group_id"])
        listed = group.get("candidate_ids")
        complete_value = group.get("candidate_ids_complete", True)
        if not isinstance(listed, list) or not isinstance(complete_value, bool):
            fail("review-groups.jsonl", f"{group_id} 缺少候选摘要", "使用当前工具重新执行 scan")
        complete = complete_value
        typed = [value for value in listed if isinstance(value, str) and value]
        members = valid_groups[group_id]
        if (
            len(typed) != len(listed)
            or len(set(typed)) != len(typed)
            or (complete and set(typed) != set(members))
            or (not complete and not set(typed) <= set(members))
        ):
            fail(
                "review-groups.jsonl",
                f"{group_id} 的候选摘要与 locations 不一致",
                "使用当前工具重新执行 scan",
            )
    return valid_groups


def _decision_rows(
    path: Path,
    groups: Sequence[Mapping[str, JsonValue]],
    locations: Sequence[Mapping[str, JsonValue]],
) -> dict[str, dict[str, JsonValue]]:
    group_members = _group_members(groups, locations)
    valid_candidates = {candidate for members in group_members.values() for candidate in members}
    output: dict[str, dict[str, JsonValue]] = {}
    for line_number, row in enumerate(read_jsonl(path, "审核决定 JSONL"), start=1):
        target = row.get("target")
        owner = row.get("owner")
        if not isinstance(target, str) or ":" not in target:
            fail(
                str(path),
                f"第 {line_number} 行 target 无效",
                "使用 group:<group_id> 或 candidate:<candidate_id>",
            )
        kind, natural_id = target.split(":", 1)
        valid = (
            natural_id in group_members
            if kind == "group"
            else natural_id in valid_candidates
            if kind == "candidate"
            else False
        )
        if not valid:
            fail(
                str(path), f"第 {line_number} 行 target 不存在", "只引用当前 review-groups.jsonl 中的自然 ID"
            )
        if target in output:
            fail(str(path), f"{target} 出现重复决定", "每个目标只保留一条决定")
        if not isinstance(owner, str) or owner not in _OWNERS:
            fail(str(path), f"{target} 的 owner 无效", "使用 rules、generic、exclude 或 unresolved")
        output[target] = row
    for group_id, candidate_ids in group_members.items():
        group_target = f"group:{group_id}"
        if group_target not in output:
            continue
        overlaps = sorted(
            candidate_id for candidate_id in candidate_ids if f"candidate:{candidate_id}" in output
        )
        if overlaps:
            fail(
                str(path),
                f"{group_target} 与成员 candidate:{overlaps[0]} 同时有决定",
                "删除组决定或全部成员决定中的一方",
            )
    return output


def _members(args: argparse.Namespace) -> int:
    survey_root = require_directory(args.survey, "survey 作业目录")
    group_id = cast(str, args.group_id)
    if not group_id.strip():
        fail("--group-id", "group_id 不能为空", "使用 review-groups.jsonl 中的自然 group_id")
    protect_outputs([args.output], inputs=[survey_root], replace=args.replace)
    _survey, locations, groups, _baseline = load_survey(survey_root)
    members_by_group = _group_members(groups, locations)
    if group_id not in members_by_group:
        fail("--group-id", f"关系组 {group_id} 不存在", "使用 review-groups.jsonl 中的自然 group_id")
    members = [
        location
        for location in locations
        if location.get("classification") == "review" and location.get("review_group_id") == group_id
    ]
    atomic_write_text(args.output, json_lines(members), replace=args.replace)
    print(f"关系组 {group_id}：已导出 {len(members)} 个完整位置。")
    print(f"成员文件：{display_path(args.output)}")
    return 0


def _non_blank(value: JsonValue) -> str | None:
    return value if isinstance(value, str) and value.strip() else None


def _generic_evidence(row: Mapping[str, JsonValue]) -> tuple[dict[str, JsonValue] | None, list[str]]:
    value = row.get("generic_evidence")
    if not isinstance(value, dict):
        return None, list(GENERIC_EVIDENCE_FIELDS)
    missing = [field for field in GENERIC_EVIDENCE_FIELDS if _non_blank(value.get(field)) is None]
    return dict(value), missing


def _rules_toml(rules: Sequence[Mapping[str, JsonValue]]) -> str:
    if not rules:
        return "rule = []\n"
    lines: list[str] = []
    for rule in rules:
        lines.append("[[rule]]")
        for key in ("file", "plugin", "code", "parameter", "path", "decode_json", "pattern"):
            if key not in rule:
                continue
            value = rule[key]
            if isinstance(value, str):
                lines.append(f"{key} = {toml_string(value)}")
            elif isinstance(value, bool):
                lines.append(f"{key} = {'true' if value else 'false'}")
            elif isinstance(value, int):
                lines.append(f"{key} = {value}")
            else:
                fail("review-groups.jsonl", f"规则字段 {key} 类型无效", "使用当前工具重新执行 scan")
        lines.append("")
    return "\n".join(lines)


def _dialogue_toml() -> str:
    """姓名 wrapper 只由译前检查按自然 ID 保护，不建立外形驱动的全局规则。"""

    return "rule = []\n"


def _generic_recipe(location: Mapping[str, JsonValue]) -> dict[str, JsonValue] | None:
    kind = location.get("generic_kind")
    locator = location.get("generic_locator")
    physical_file = location.get("physical_file")
    source_text = location.get("source_text")
    candidate_id = location.get("candidate_id")
    if (
        kind not in {"javascript_literal", "plain_text_line"}
        or not isinstance(locator, dict)
        or not isinstance(physical_file, str)
        or not isinstance(source_text, str)
        or not isinstance(candidate_id, str)
    ):
        return None
    line = locator.get("line")
    if not isinstance(line, int) or isinstance(line, bool) or line <= 0:
        return None
    recipe: dict[str, JsonValue] = {
        "candidate_id": candidate_id,
        "physical_file": physical_file,
        "kind": kind,
        "source": source_text,
        "source_line": line,
    }
    if kind == "javascript_literal":
        start = locator.get("start")
        end = locator.get("end")
        quote = locator.get("quote")
        if (
            not isinstance(start, int)
            or isinstance(start, bool)
            or start < 0
            or not isinstance(end, int)
            or isinstance(end, bool)
            or end <= start
            or quote not in {"'", '"'}
        ):
            return None
        recipe.update({"start": start, "end": end, "quote": cast(str, quote)})
    return recipe


def _generic_materials(
    plans: Sequence[Mapping[str, JsonValue]],
) -> tuple[dict[str, str], dict[str, JsonValue]]:
    groups_by_source: dict[str, list[dict[str, object]]] = {}
    decisions: list[dict[str, JsonValue]] = []
    target_by_candidate: dict[str, str] = {}
    for plan in plans:
        target = cast(str, plan["target"])
        raw_locations = cast(list[JsonValue], plan["locations"])
        locations = [value for value in raw_locations if isinstance(value, dict)]
        by_scope: dict[tuple[str, str], list[dict[str, JsonValue]]] = {}
        for location in locations:
            recipe = _generic_recipe(location)
            if recipe is None:
                raise AssertionError("Generic 计划包含未验收的往返类型")
            by_scope.setdefault((cast(str, recipe["physical_file"]), cast(str, recipe["kind"])), []).append(
                recipe
            )
        multiple_scopes = len(by_scope) > 1
        for (physical_file, kind), source_recipes in sorted(by_scope.items()):
            group_id = f"{target}|{physical_file}|{kind}" if multiple_scopes else target
            groups_by_source.setdefault(physical_file, []).append(
                {
                    "id": group_id,
                    "kind": kind,
                    "recipes": source_recipes,
                }
            )
        decisions.append(
            {
                "target": target,
                "candidate_ids": cast(list[JsonValue], plan["candidate_ids"]),
                "evidence": cast(dict[str, JsonValue], plan["evidence"]),
            }
        )
        target_by_candidate.update(
            {cast(str, candidate_id): target for candidate_id in cast(list[JsonValue], plan["candidate_ids"])}
        )
    files: dict[str, str] = {}
    sources: list[dict[str, JsonValue]] = []
    recipes: list[dict[str, JsonValue]] = []
    for physical_file in sorted(groups_by_source, key=lambda value: value.encode("utf-8")):
        input_file = f"generic/input/{physical_file}.jsonl"
        input_relative = f"{physical_file}.jsonl"
        serialized_groups: list[dict[str, JsonValue]] = []
        unit_count = 0
        for group_line, raw_group in enumerate(groups_by_source[physical_file], start=1):
            group_id = cast(str, raw_group["id"])
            kind = cast(str, raw_group["kind"])
            source_recipes = cast(list[dict[str, JsonValue]], raw_group["recipes"])
            units: list[dict[str, JsonValue]] = []
            for unit_number, recipe in enumerate(source_recipes, start=1):
                unit_id = cast(str, recipe["candidate_id"])
                units.append({"id": unit_id, "text": cast(str, recipe["source"])})
                recipes.append(
                    {
                        "target": target_by_candidate[unit_id],
                        **recipe,
                        "input_file": input_file,
                        "group_id": group_id,
                        "group_line": group_line,
                        "unit_id": unit_id,
                        "unit_number": unit_number,
                        "manual_id": f"{input_relative}:line{group_line}:unit{unit_number}:text",
                    }
                )
            unit_count += len(units)
            serialized_groups.append({"id": group_id, "kind": kind, "units": units})
        files[input_file] = json_lines(serialized_groups)
        sources.append(
            {
                "physical_file": physical_file,
                "input_file": input_file,
                "groups": len(serialized_groups),
                "units": unit_count,
            }
        )
    manifest: dict[str, JsonValue] = {
        "sources": sources,
        "decisions": decisions,
        "recipes": recipes,
    }
    return files, manifest


def _finalize(args: argparse.Namespace) -> int:
    started = time.perf_counter()
    survey_root = require_directory(args.survey, "survey 作业目录")
    decisions_argument = cast(Path | None, args.decisions)
    decisions_path = require_file(
        decisions_argument if decisions_argument is not None else survey_root / "ownership-decisions.jsonl",
        "审核决定 JSONL",
    )
    protect_outputs(
        [args.output],
        inputs=[survey_root, decisions_path],
        replace=args.replace,
    )
    survey, locations, groups, baseline = load_survey(survey_root)
    verify_source_baseline(survey, baseline)
    decisions = _decision_rows(decisions_path, groups, locations)
    group_members = _group_members(groups, locations)
    locations_by_id = {
        str(item["candidate_id"]): item for item in locations if isinstance(item.get("candidate_id"), str)
    }
    accumulated_rules: dict[str, dict[str, object]] = {}
    dispositions: list[dict[str, JsonValue]] = []
    unresolved: list[dict[str, JsonValue]] = []
    missing_targets: list[str] = []
    generic_plan: list[dict[str, JsonValue]] = []
    decided_candidates: set[str] = set()

    for group in groups:
        group_id = group.get("group_id")
        if not isinstance(group_id, str) or not group_id:
            fail(str(survey_root / "review-groups.jsonl"), "关系组结构无效", "使用当前工具重新执行 scan")
        typed_ids = group_members[group_id]
        if any(value not in locations_by_id for value in typed_ids):
            fail(
                str(survey_root / "review-groups.jsonl"),
                f"关系组 {group_id} 引用未知位置",
                "使用当前工具重新执行 scan",
            )
        group_target = f"group:{group_id}"
        selections: list[tuple[str, dict[str, JsonValue], list[str]]] = []
        group_row = decisions.get(group_target)
        if group_row is not None:
            selections.append((group_target, group_row, typed_ids))
        else:
            for candidate_id in typed_ids:
                target = f"candidate:{candidate_id}"
                row = decisions.get(target)
                if row is None:
                    missing_targets.append(target)
                    unresolved.append({"target": target, "reason": "missing_decision"})
                    decided_candidates.add(candidate_id)
                else:
                    selections.append((target, row, [candidate_id]))

        for target, row, selected_ids in selections:
            overlap = decided_candidates.intersection(selected_ids)
            if overlap:
                fail(
                    str(survey_root / "review-groups.jsonl"),
                    f"位置 {min(overlap)} 被决定两次",
                    "重新执行 scan；每个位置只能有一个决定",
                )
            decided_candidates.update(selected_ids)
            owner = cast(str, row["owner"])
            if owner == "rules":
                for candidate_id in selected_ids:
                    location = locations_by_id[candidate_id]
                    rule = location.get("rule")
                    expected_id = location.get("expected_manual_id")
                    if not isinstance(rule, dict) or not isinstance(expected_id, str):
                        fail(
                            str(decisions_path),
                            f"{target} 没有可证明且可物化的 Rules 方案",
                            "改为 unresolved，或完成 Generic 的全部证据",
                        )
                    key = json.dumps(rule, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
                    accumulator = accumulated_rules.setdefault(
                        key,
                        {
                            "rule": dict(rule),
                            "candidate_ids": [],
                            "locations": [],
                            "expected_manual_ids": set(),
                            "targets": set(),
                        },
                    )
                    cast(list[str], accumulator["candidate_ids"]).append(candidate_id)
                    cast(list[str], accumulator["locations"]).append(str(location["location"]))
                    cast(set[str], accumulator["expected_manual_ids"]).add(expected_id)
                    cast(set[str], accumulator["targets"]).add(target)
                dispositions.append({"target": target, "owner": "rules", "candidate_ids": selected_ids})
            elif owner == "generic":
                evidence, missing = _generic_evidence(row)
                if missing:
                    unresolved.append(
                        {
                            "target": target,
                            "reason": "generic_evidence_incomplete",
                            "missing": missing,
                        }
                    )
                    continue
                assert evidence is not None
                unsupported = [
                    candidate_id
                    for candidate_id in selected_ids
                    if _generic_recipe(locations_by_id[candidate_id]) is None
                ]
                if unsupported:
                    unresolved.append(
                        {
                            "target": target,
                            "reason": "generic_roundtrip_not_supported",
                            "candidate_ids": unsupported,
                        }
                    )
                    continue
                dispositions.append(
                    {
                        "target": target,
                        "owner": "generic",
                        "candidate_ids": selected_ids,
                        "evidence": evidence,
                    }
                )
                generic_plan.append(
                    {
                        "target": target,
                        "candidate_ids": selected_ids,
                        "locations": [locations_by_id[value] for value in selected_ids],
                        "evidence": evidence,
                    }
                )
            elif owner == "exclude":
                reason = _non_blank(row.get("reason"))
                evidence = _non_blank(row.get("evidence"))
                if reason is None or evidence is None:
                    unresolved.append({"target": target, "reason": "exclusion_evidence_incomplete"})
                    continue
                dispositions.append(
                    {
                        "target": target,
                        "owner": "exclude",
                        "candidate_ids": selected_ids,
                        "reason": reason,
                        "evidence": evidence,
                    }
                )
            else:
                unresolved.append(
                    {
                        "target": target,
                        "reason": _non_blank(row.get("reason")) or "Agent 尚未确认唯一所有者",
                    }
                )

    rules: list[dict[str, JsonValue]] = []
    manifest: list[dict[str, JsonValue]] = []
    for accumulator in accumulated_rules.values():
        rule = cast(dict[str, JsonValue], accumulator["rule"])
        rules.append(rule)
        manifest.append(
            {
                "rule_number": len(rules),
                "rule": rule,
                "candidate_ids": cast(list[str], accumulator["candidate_ids"]),
                "locations": cast(list[str], accumulator["locations"]),
                "expected_manual_ids": sorted(cast(set[str], accumulator["expected_manual_ids"])),
                "targets": sorted(cast(set[str], accumulator["targets"])),
            }
        )

    engine = survey.get("engine")
    if engine not in {"mv", "mz"}:
        fail(str(survey_root / "survey.json"), "engine 无效", "重新运行 scan")
    builtin_projection = project_builtin_units(locations)
    rules_projection = project_rule_units(
        manifest,
        locations_by_id,
        validate_expected_manual_ids=False,
    )
    projected_rule_ids: dict[int, list[str]] = {}
    for item in rules_projection:
        projected_rule_ids.setdefault(cast(int, item["rule_number"]), []).append(cast(str, item["manual_id"]))
    for manifest_item in manifest:
        manifest_item["expected_manual_ids"] = sorted(
            projected_rule_ids.get(cast(int, manifest_item["rule_number"]), [])
        )
    unit_projection = [*builtin_projection, *rules_projection]
    projected_ids = [cast(str, item["manual_id"]) for item in unit_projection]
    if len(projected_ids) != len(set(projected_ids)):
        fail(str(args.output), "Unit 投影产生重复 Manual ID", "重新执行 scan 并修正冲突的来源")
    expected_ownership: list[dict[str, JsonValue]] = []
    for item in sorted(unit_projection, key=lambda value: cast(str, value["manual_id"])):
        ownership: dict[str, JsonValue] = {
            "manual_id": cast(str, item["manual_id"]),
            "owner": cast(str, item["owner"]),
        }
        if item.get("owner") == "rules":
            ownership["rule_number"] = cast(int, item["rule_number"])
        expected_ownership.append(ownership)
    review_candidate_ids = {
        str(item["candidate_id"])
        for item in locations
        if item.get("classification") == "review" and isinstance(item.get("candidate_id"), str)
    }
    undisposed = sorted(review_candidate_ids - decided_candidates)
    if undisposed:
        fail(str(survey_root), f"{len(undisposed)} 个审核位置没有进入任何关系组", "使用当前工具重新执行 scan")
    complete = not unresolved and not missing_targets
    coverage: dict[str, JsonValue] = {
        "complete": complete,
        "engine": engine,
        "builtin_candidate_ids": [
            str(item["candidate_id"]) for item in locations if item.get("classification") == "builtin"
        ],
        "resource_reference_candidate_ids": [
            str(item["candidate_id"])
            for item in locations
            if item.get("classification") == "resource_reference"
        ],
        "structural_whitespace_candidate_ids": [
            str(item["candidate_id"])
            for item in locations
            if item.get("classification") == "structural_whitespace"
        ],
        "dispositions": dispositions,
        "unresolved": unresolved,
        "missing_targets": missing_targets,
        "expected_ownership": expected_ownership,
        "unit_projection": unit_projection,
        "counts": {
            "locations": len(locations),
            "review_groups": len(groups),
            "decisions": len(decisions),
            "rules": len(rules),
            "generic_groups": len(generic_plan),
            "unresolved": len(unresolved),
        },
    }
    metrics = {
        "local_command_elapsed_ms": round((time.perf_counter() - started) * 1000),
        "explicit_decisions": len(decisions),
        "review_groups": len(groups),
        "generated_rule_objects": len(rules),
        "handwritten_rule_objects_required": 0,
        "local_commands_completed": 2,
        "external_request_wait_ms": 0,
    }
    generic_files, generic_manifest = _generic_materials(generic_plan)
    atomic_write_directory(
        args.output,
        {
            "dialogue-rules.toml": _dialogue_toml(),
            "rules.toml": _rules_toml(rules),
            "rules-manifest.json": _json_text({"rules": manifest}),
            "coverage.json": _json_text(coverage),
            "generic/manifest.json": _json_text(generic_manifest),
            "agent-work-metrics.json": _json_text(metrics),
            **generic_files,
        },
        replace=args.replace,
    )
    state = "完整" if complete else "待继续审核"
    print(f"调查决定已生成：{state}；Rules {len(rules)} 条，未确认 {len(unresolved)} 项。")
    print(f"计划目录：{display_path(args.output)}")
    return 0


def _normalized_toml_rules(path: Path) -> list[dict[str, JsonValue]]:
    source = require_file(path, "Rules TOML")
    try:
        root = cast(
            dict[str, object],
            tomllib.loads(source.read_text(encoding="utf-8-sig")),
        )
    except tomllib.TOMLDecodeError as error:
        fail(str(source), f"TOML 无效：{error}", "重新运行 finalize 生成规则")
    rules_value = root.get("rule")
    if set(root) != {"rule"} or not isinstance(rules_value, list):
        fail(str(source), "Rules TOML 根结构无效", "重新运行 finalize 生成规则")
    typed_rules = cast(list[object], rules_value)
    output: list[dict[str, JsonValue]] = []
    for number, raw_item in enumerate(typed_rules, start=1):
        if not isinstance(raw_item, dict):
            fail(str(source), f"第 {number} 条规则不是 object", "重新运行 finalize 生成规则")
        item = cast(dict[str, JsonValue], raw_item)
        output.append(dict(item))
    return output


def _ownership_rows(path: Path) -> list[dict[str, JsonValue]]:
    rows = read_jsonl(path, "ATT 所有权 JSONL")
    seen: set[str] = set()
    for number, row in enumerate(rows, start=1):
        owner = row.get("owner")
        allowed = {"manual_id", "owner"} if owner == "builtin" else {"manual_id", "owner", "rule_number"}
        validate_object_keys(row, f"所有权第 {number} 行", allowed)
        manual_id = row.get("manual_id")
        if not isinstance(manual_id, str) or not manual_id or owner not in {"builtin", "rules"}:
            fail(str(path), f"第 {number} 行 manual_id/owner 无效", "使用当前 ATT 重新导出完整所有权")
        if owner == "rules" and (
            not isinstance(row.get("rule_number"), int) or isinstance(row.get("rule_number"), bool)
        ):
            fail(str(path), f"第 {number} 行缺少自然 rule_number", "使用当前 ATT 重新导出完整所有权")
        if manual_id in seen:
            fail(str(path), f"manual_id {manual_id} 重复", "检查 ATT 当前 Extract 状态后重新导出")
        seen.add(manual_id)
    return rows


def _audit(args: argparse.Namespace) -> int:
    started = time.perf_counter()
    survey_root = require_directory(args.survey, "survey 作业目录")
    plan_root = require_directory(args.plan, "finalize 计划目录")
    ownership_path = require_file(args.ownership, "ATT 所有权 JSONL")
    protect_outputs(
        [args.output],
        inputs=[survey_root, plan_root, ownership_path],
        replace=args.replace,
    )
    survey = read_json_object(survey_root / "survey.json", "survey.json", allowed_root=survey_root)
    coverage = read_json_object(plan_root / "coverage.json", "coverage.json", allowed_root=plan_root)
    survey_engine = survey.get("engine")
    if survey_engine not in {"mv", "mz"} or coverage.get("engine") != survey_engine:
        fail(
            str(survey_root / "survey.json"),
            "survey 引擎与 finalize 计划不一致",
            "使用生成该计划的 survey 作业目录",
        )
    manifest_root = read_json_object(
        plan_root / "rules-manifest.json", "rules-manifest.json", allowed_root=plan_root
    )
    manifest_value = manifest_root.get("rules")
    if not isinstance(manifest_value, list):
        fail(str(plan_root / "rules-manifest.json"), "缺少 rules 数组", "重新运行 finalize")
    manifest_rules: list[dict[str, JsonValue]] = []
    for number, item in enumerate(manifest_value, start=1):
        if (
            not isinstance(item, dict)
            or item.get("rule_number") != number
            or not isinstance(item.get("rule"), dict)
        ):
            fail(
                str(plan_root / "rules-manifest.json"),
                f"第 {number} 项与自然规则序号不一致",
                "重新运行 finalize",
            )
        rule_value = item.get("rule")
        assert isinstance(rule_value, dict)
        manifest_rules.append(dict(rule_value))
    toml_rules = _normalized_toml_rules(plan_root / "rules.toml")
    if toml_rules != manifest_rules:
        fail(str(plan_root), "Rules TOML 与 manifest 逐条不一致", "不要手工改写计划目录；重新运行 finalize")
    expected_value = coverage.get("expected_ownership")
    if not isinstance(expected_value, list):
        fail(str(plan_root / "coverage.json"), "缺少 expected_ownership", "重新运行 finalize")
    expected: dict[str, tuple[str, int | None]] = {}
    for number, item in enumerate(expected_value, start=1):
        if not isinstance(item, dict):
            fail(
                str(plan_root / "coverage.json"),
                f"expected_ownership 第 {number} 项无效",
                "重新运行 finalize",
            )
        manual_id = item.get("manual_id")
        owner = item.get("owner")
        rule_number = item.get("rule_number")
        if not isinstance(manual_id, str) or owner not in {"builtin", "rules"}:
            fail(
                str(plan_root / "coverage.json"),
                f"expected_ownership 第 {number} 项无效",
                "重新运行 finalize",
            )
        if manual_id in expected:
            fail(str(plan_root / "coverage.json"), f"预期 manual_id {manual_id} 重复", "修正冲突的调查决定")
        expected[manual_id] = (
            str(owner),
            rule_number if isinstance(rule_number, int) and not isinstance(rule_number, bool) else None,
        )
    actual_rows = _ownership_rows(ownership_path)
    actual = {
        str(row["manual_id"]): (
            str(row["owner"]),
            cast(int, row["rule_number"])
            if isinstance(row.get("rule_number"), int) and not isinstance(row.get("rule_number"), bool)
            else None,
        )
        for row in actual_rows
    }
    missing = sorted(set(expected) - set(actual))
    unexpected = sorted(set(actual) - set(expected))
    mismatched = sorted(
        manual_id for manual_id in set(expected) & set(actual) if expected[manual_id] != actual[manual_id]
    )
    complete = coverage.get("complete") is True and not missing and not unexpected and not mismatched
    findings: list[dict[str, JsonValue]] = []
    for kind, values in (
        ("missing", missing),
        ("unexpected", unexpected),
        ("mismatched", mismatched),
    ):
        if values:
            findings.append(
                {
                    "kind": kind,
                    "count": len(values),
                    "first": values[0],
                    "samples": values[:5],
                }
            )
    report: dict[str, JsonValue] = {
        "complete": complete,
        "ownership_entries": len(actual),
        "builtin_entries": sum(owner == "builtin" for owner, _ in actual.values()),
        "rules_entries": sum(owner == "rules" for owner, _ in actual.values()),
        "rules": len(toml_rules),
        "missing": len(missing),
        "unexpected": len(unexpected),
        "mismatched": len(mismatched),
        "findings": findings,
        "agent_work_metrics": {
            "local_command_elapsed_ms": round((time.perf_counter() - started) * 1000),
            "local_commands_completed": 3,
            "ownership_rows_compared": len(actual),
            "external_request_wait_ms": 0,
        },
    }
    write_json(args.output, report, replace=args.replace)
    state = "完整" if complete else "覆盖不完整；Translate 可运行，但不能宣称来源覆盖完整"
    print(f"所有权审计：{state}；精确核对 {len(actual)} 个 Manual ID。")
    print(f"审计报告：{display_path(args.output)}")
    return 0


def _main(args: argparse.Namespace) -> int:
    if args.command == "scan":
        return _scan(args)
    if args.command == "finalize":
        return _finalize(args)
    if args.command == "members":
        return _members(args)
    return _audit(args)


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _main(parsed))
