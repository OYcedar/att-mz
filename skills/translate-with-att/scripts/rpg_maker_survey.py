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
    print_published_completion,
    protect_outputs,
    require_directory,
    require_file,
    run_cli,
    toml_string,
    validate_object_keys,
    write_json,
)
from att_toolbox.coverage import coverage_projection
from att_toolbox.generic_mapping import (
    generic_mapping_groups,
    generic_materials,
    generic_recipe,
    validate_generic_evidence,
)
from att_toolbox.survey import (
    GENERIC_EVIDENCE_FIELDS,
    json_lines,
    load_survey,
    read_jsonl,
    scan_game,
    survey_game_root,
    verify_source_baseline,
)
from att_toolbox.survey_projection import project_builtin_units, project_rule_units, read_rules_manifest

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
    members.add_argument("--output", type=Path, required=True, help="可替换组决定的 candidate 决定模板")
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


def _decision_member(location: Mapping[str, JsonValue]) -> dict[str, JsonValue]:
    if not all(isinstance(location.get(field), str) for field in ("source", "location", "source_text")):
        fail("locations.jsonl", "审核位置缺少自然来源、位置或正文", "使用当前工具重新执行 scan")
    return dict(location)


def _ownership_decision_template(
    groups: Sequence[Mapping[str, JsonValue]],
    locations: Sequence[Mapping[str, JsonValue]],
    game_root: str,
) -> list[dict[str, JsonValue]]:
    members = _group_members(groups, locations)
    locations_by_id = {
        cast(str, item["candidate_id"]): item
        for item in locations
        if isinstance(item.get("candidate_id"), str)
    }
    rows: list[dict[str, JsonValue]] = []
    for group in groups:
        group_id = group.get("group_id")
        if not isinstance(group_id, str) or not group_id:
            fail("review-groups.jsonl", "关系组缺少自然 group_id", "使用当前工具重新执行 scan")
        rows.append(
            {
                "target": f"group:{group_id}",
                "game_root": game_root,
                "members": [_decision_member(locations_by_id[value]) for value in members[group_id]],
                "owner": "unresolved",
            }
        )
    return rows


def _scan(args: argparse.Namespace) -> int:
    # scan_game 会自行确认真实内容根；这里先保护显式游戏根，避免输出写进来源树。
    game_root = require_directory(args.game, "游戏目录")
    protect_outputs(
        [args.output],
        inputs=[game_root],
        forbidden_roots=[game_root],
        replace=args.replace,
    )
    bundle = scan_game(game_root)
    locations_text = json_lines(bundle.locations)
    groups_text = json_lines(bundle.review_groups)
    game_root_value = cast(str, bundle.summary["game_root"])
    decisions_text = json_lines(
        _ownership_decision_template(bundle.review_groups, bundle.locations, game_root_value)
    )
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
    output_display = display_path(args.output)
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
    print_published_completion(
        "\n".join(
            [
                (
                    f"已完成一次 {str(survey['engine']).upper()} 调查："
                    f"位置 {survey['locations']} 个，阅读 packet {survey['review_packets']} 个，"
                    f"可决定关系组 {survey['review_groups']} 个。"
                ),
                f"调查目录：{output_display}",
                "所有权决定模板：ownership-decisions.jsonl（默认 unresolved，只修改已经确认的组）",
            ]
        ),
        object_name=f"Survey 调查目录 {output_display}",
        impact="Survey 调查目录已经完整发布并可直接审核；最终完成提示未能显示",
        help_text="直接打开该 Survey 调查目录继续审核，无需重新运行 scan",
    )
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
    game_root: str,
) -> dict[str, dict[str, JsonValue]]:
    group_members = _group_members(groups, locations)
    valid_candidates = {candidate for members in group_members.values() for candidate in members}
    locations_by_id = {
        cast(str, item["candidate_id"]): item
        for item in locations
        if isinstance(item.get("candidate_id"), str)
    }
    expected_members: dict[str, list[dict[str, JsonValue]]] = {
        f"group:{group_id}": [_decision_member(locations_by_id[value]) for value in candidate_ids]
        for group_id, candidate_ids in group_members.items()
    }
    expected_members.update(
        {
            f"candidate:{candidate_id}": [_decision_member(locations_by_id[candidate_id])]
            for candidate_id in valid_candidates
        }
    )
    output: dict[str, dict[str, JsonValue]] = {}
    for line_number, row in enumerate(read_jsonl(path, "审核决定 JSONL"), start=1):
        validate_object_keys(
            row,
            f"{path} 第 {line_number} 行",
            {"target", "game_root", "members", "owner", "generic_evidence", "reason", "evidence"},
        )
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
        if row.get("game_root") != game_root or row.get("members") != expected_members[target]:
            fail(
                str(path),
                f"第 {line_number} 行不属于当前 survey 的来源位置集合",
                "从当前 ownership-decisions.jsonl 复制目标行并只填写决定与证据",
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
    survey, locations, groups, _baseline = load_survey(survey_root)
    game_root = survey_game_root(survey)
    group_id = cast(str, args.group_id)
    if not group_id.strip():
        fail("--group-id", "group_id 不能为空", "使用 review-groups.jsonl 中的自然 group_id")
    protect_outputs(
        [args.output],
        inputs=[survey_root, game_root],
        forbidden_roots=[game_root],
        replace=args.replace,
    )
    members_by_group = _group_members(groups, locations)
    if group_id not in members_by_group:
        fail("--group-id", f"关系组 {group_id} 不存在", "使用 review-groups.jsonl 中的自然 group_id")
    members = [
        location
        for location in locations
        if location.get("classification") == "review" and location.get("review_group_id") == group_id
    ]
    candidate_decisions = [
        {
            "target": f"candidate:{cast(str, location['candidate_id'])}",
            "game_root": cast(str, survey["game_root"]),
            "members": [_decision_member(location)],
            "owner": "unresolved",
        }
        for location in members
    ]
    output_display = display_path(args.output)
    atomic_write_text(args.output, json_lines(candidate_decisions), replace=args.replace)
    print_published_completion(
        "\n".join(
            [
                f"关系组 {group_id}：已导出 {len(members)} 条可填写的 candidate 决定。",
                f"候选决定文件：{output_display}",
            ]
        ),
        object_name=f"candidate 决定文件 {output_display}",
        impact="candidate 决定文件已经完整发布并可直接填写；最终完成提示未能显示",
        help_text="直接填写该 candidate 决定文件，无需重新运行 members",
    )
    return 0


def _non_blank(value: JsonValue) -> str | None:
    return value if isinstance(value, str) and value.strip() else None


def _generic_evidence(
    row: Mapping[str, JsonValue],
    candidate_ids: Sequence[str],
) -> tuple[dict[str, JsonValue] | None, list[str]]:
    value = row.get("generic_evidence")
    if not isinstance(value, dict):
        return None, list(GENERIC_EVIDENCE_FIELDS)
    missing = [
        field
        for field in GENERIC_EVIDENCE_FIELDS
        if (
            not isinstance(value.get(field), dict)
            if field == "extract_group_unit_write_back_mapping"
            else _non_blank(value.get(field)) is None
        )
    ]
    if missing:
        return None, missing
    return validate_generic_evidence(value, candidate_ids, "审核决定 generic_evidence"), []


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


def _finalize(args: argparse.Namespace) -> int:
    started = time.perf_counter()
    survey_root = require_directory(args.survey, "survey 作业目录")
    decisions_argument = cast(Path | None, args.decisions)
    decisions_path = require_file(
        decisions_argument if decisions_argument is not None else survey_root / "ownership-decisions.jsonl",
        "审核决定 JSONL",
    )
    survey, locations, groups, baseline = load_survey(survey_root)
    game_root = survey_game_root(survey)
    protect_outputs(
        [args.output],
        inputs=[survey_root, decisions_path, game_root],
        forbidden_roots=[game_root],
        replace=args.replace,
    )
    verify_source_baseline(survey, baseline)
    decisions = _decision_rows(decisions_path, groups, locations, cast(str, survey["game_root"]))
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
                evidence, missing = _generic_evidence(row, selected_ids)
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
                    if generic_recipe(locations_by_id[candidate_id]) is None
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
    if not isinstance(engine, str) or engine not in {"mv", "mz"}:
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
            "generic_groups": sum(
                len(generic_mapping_groups(cast(dict[str, JsonValue], plan["evidence"])))
                for plan in generic_plan
            ),
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
    generic_files, generic_manifest = generic_materials(generic_plan)
    output_display = display_path(args.output)
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
    print_published_completion(
        "\n".join(
            [
                f"调查决定已生成：{state}；Rules {len(rules)} 条，未确认 {len(unresolved)} 项。",
                f"计划目录：{output_display}",
            ]
        ),
        object_name=f"Survey 计划目录 {output_display}",
        impact="Survey 计划目录已经完整发布并可直接使用；最终完成提示未能显示",
        help_text="直接使用该计划目录继续后续步骤，无需重新运行 finalize",
    )
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
        if (
            not isinstance(manual_id, str)
            or not manual_id
            or not isinstance(owner, str)
            or owner not in {"builtin", "rules"}
        ):
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
    survey, locations, groups, baseline = load_survey(survey_root)
    game_root = survey_game_root(survey)
    protect_outputs(
        [args.output],
        inputs=[survey_root, plan_root, ownership_path, game_root],
        forbidden_roots=[game_root],
        replace=args.replace,
    )
    verify_source_baseline(survey, baseline)
    coverage_path = plan_root / "coverage.json"
    rules_manifest = read_rules_manifest(plan_root / "rules-manifest.json")
    projection, _generic_candidates, coverage_complete, _generic_plans = coverage_projection(
        coverage_path,
        survey,
        locations,
        groups,
        rules_manifest,
    )
    manifest_rules = [cast(dict[str, JsonValue], item["rule"]) for item in rules_manifest]
    toml_rules = _normalized_toml_rules(plan_root / "rules.toml")
    if toml_rules != manifest_rules:
        fail(str(plan_root), "Rules TOML 与 manifest 逐条不一致", "不要手工改写计划目录；重新运行 finalize")
    expected = {
        manual_id: (
            cast(str, item["owner"]),
            cast(int, item["rule_number"]) if item.get("owner") == "rules" else None,
        )
        for manual_id, item in projection.items()
    }
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
    complete = coverage_complete and not missing and not unexpected and not mismatched
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
    output_display = display_path(args.output)
    write_json(args.output, report, replace=args.replace)
    state = "完整" if complete else "覆盖不完整；Translate 可运行，但不能宣称来源覆盖完整"
    print_published_completion(
        "\n".join(
            [
                f"所有权审计：{state}；精确核对 {len(actual)} 个 Manual ID。",
                f"审计报告：{output_display}",
            ]
        ),
        object_name=f"Survey 所有权审计报告 {output_display}",
        impact="Survey 所有权审计报告已经完整发布并可直接查看；最终完成提示未能显示",
        help_text="直接查看该审计报告并按结果继续，无需重新运行 audit",
    )
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
