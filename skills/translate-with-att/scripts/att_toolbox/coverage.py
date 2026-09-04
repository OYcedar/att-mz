"""校验 Survey finalize coverage 并重建唯一 ATT Unit 投影。"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

from att_skill_tools import JsonValue, fail, read_json_object, validate_object_keys

from .generic_mapping import (
    validate_generic_evidence,
    validate_generic_group_placement,
)
from .survey_projection import project_builtin_units, project_rule_units


def coverage_projection(
    path: Path,
    survey: Mapping[str, JsonValue],
    locations: Sequence[Mapping[str, JsonValue]],
    groups: Sequence[Mapping[str, JsonValue]],
    rules_manifest: Sequence[Mapping[str, JsonValue]],
) -> tuple[
    dict[str, dict[str, JsonValue]],
    set[str],
    bool,
    list[dict[str, JsonValue]],
]:
    coverage = read_json_object(path, "finalize coverage.json")
    engine = survey.get("engine")
    if not isinstance(engine, str) or engine not in {"mv", "mz"} or coverage.get("engine") != engine:
        fail(str(path), "coverage 与 survey 引擎不一致", "使用同一次 survey finalize 生成的 coverage.json")
    allowed_classifications = {"builtin", "review", "resource_reference", "structural_whitespace"}
    for number, item in enumerate(locations, start=1):
        classification = item.get("classification")
        if not isinstance(classification, str) or classification not in allowed_classifications:
            fail(str(path), f"survey 第 {number} 个位置 classification 无效", "重新运行 survey scan/finalize")
    counts = coverage.get("counts")
    if (
        not isinstance(counts, dict)
        or counts.get("locations") != len(locations)
        or counts.get("review_groups") != len(groups)
        or counts.get("rules") != len(rules_manifest)
    ):
        fail(
            str(path),
            "coverage 与 survey 的位置总数不一致",
            "使用同一次 survey finalize 生成的 coverage.json",
        )

    locations_by_id = {
        cast(str, item["candidate_id"]): item
        for item in locations
        if isinstance(item.get("candidate_id"), str)
    }
    if len(locations_by_id) != len(locations):
        fail(str(path), "survey 候选身份缺失或重复", "重新运行 survey scan/finalize")
    expected_projection_rows = [
        *project_builtin_units(locations),
        *project_rule_units(rules_manifest, locations_by_id),
    ]
    expected_projection = {cast(str, item["manual_id"]): item for item in expected_projection_rows}
    if len(expected_projection) != len(expected_projection_rows):
        fail(str(path), "Survey 与 Rules manifest 投影出重复 Unit", "重新运行 survey finalize")
    classified_candidates = {
        classification: {
            cast(str, item["candidate_id"])
            for item in locations
            if item.get("classification") == classification and isinstance(item.get("candidate_id"), str)
        }
        for classification in ("builtin", "resource_reference", "structural_whitespace")
    }
    for field, classification in (
        ("builtin_candidate_ids", "builtin"),
        ("resource_reference_candidate_ids", "resource_reference"),
        ("structural_whitespace_candidate_ids", "structural_whitespace"),
    ):
        raw_values = coverage.get(field)
        if (
            not isinstance(raw_values, list)
            or any(not isinstance(value, str) for value in raw_values)
            or len(raw_values) != len(set(cast(list[str], raw_values)))
            or set(cast(list[str], raw_values)) != classified_candidates[classification]
        ):
            fail(
                str(path),
                f"{field} 与 survey 完整分类不一致",
                "使用同一次 survey finalize 生成的 coverage.json",
            )
    projection_value = coverage.get("unit_projection")
    ownership_value = coverage.get("expected_ownership")
    if not isinstance(projection_value, list) or not isinstance(ownership_value, list):
        fail(str(path), "coverage 缺少 Unit 投影或预期所有权", "重新运行 rpg_maker_survey.py finalize")

    projection: dict[str, dict[str, JsonValue]] = {}
    for number, raw in enumerate(projection_value, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"unit_projection 第 {number} 项不是 object", "重新运行 finalize")
        item = dict(raw)
        manual_id = item.get("manual_id")
        candidate_id = item.get("candidate_id")
        owner = item.get("owner")
        rule_number = item.get("rule_number")
        if (
            not isinstance(manual_id, str)
            or not manual_id
            or manual_id in projection
            or not isinstance(candidate_id, str)
            or candidate_id not in locations_by_id
            or not isinstance(owner, str)
            or owner not in {"builtin", "rules"}
            or not isinstance(item.get("source_text"), str)
            or not isinstance(item.get("source"), str)
            or item.get("content_kind") not in {"value", "lines"}
        ):
            fail(str(path), f"unit_projection 第 {number} 项身份或字段无效", "重新运行 finalize")
        if owner == "rules":
            if not isinstance(rule_number, int) or isinstance(rule_number, bool) or rule_number <= 0:
                fail(str(path), f"{manual_id} 缺少自然 rule_number", "重新运行 finalize")
        elif rule_number is not None:
            fail(str(path), f"{manual_id} 不应包含 rule_number", "重新运行 finalize")
        location = locations_by_id[candidate_id]
        if (
            location.get("source") != item["source"]
            or (owner == "builtin" and location.get("source_text") != item["source_text"])
            or (owner == "builtin" and location.get("content_kind") != item["content_kind"])
        ):
            fail(str(path), f"{manual_id} 与 survey 候选内容不一致", "使用同一次 survey 生成的 finalize 计划")
        projection[manual_id] = item

    ownership: dict[str, tuple[str, int | None]] = {}
    for number, raw in enumerate(ownership_value, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"expected_ownership 第 {number} 项不是 object", "重新运行 finalize")
        manual_id = raw.get("manual_id")
        owner = raw.get("owner")
        rule_number = raw.get("rule_number")
        if (
            not isinstance(manual_id, str)
            or manual_id in ownership
            or not isinstance(owner, str)
            or owner not in {"builtin", "rules"}
        ):
            fail(str(path), f"expected_ownership 第 {number} 项无效", "重新运行 finalize")
        normalized_rule = (
            rule_number
            if isinstance(rule_number, int) and not isinstance(rule_number, bool) and rule_number > 0
            else None
        )
        if (owner == "rules") != (normalized_rule is not None):
            fail(str(path), f"{manual_id} 的预期所有权字段矛盾", "重新运行 finalize")
        ownership[manual_id] = (owner, normalized_rule)
    projected_ownership = {
        manual_id: (
            cast(str, item["owner"]),
            cast(int, item["rule_number"]) if item.get("owner") == "rules" else None,
        )
        for manual_id, item in projection.items()
    }
    if ownership != projected_ownership:
        fail(str(path), "expected_ownership 与 unit_projection 不一致", "重新运行 finalize")
    if projection != expected_projection:
        differing = sorted(set(projection).symmetric_difference(expected_projection))
        if not differing:
            differing = sorted(
                manual_id
                for manual_id in projection
                if projection[manual_id] != expected_projection[manual_id]
            )
        detail = differing[0] if differing else "未知 Unit"
        fail(
            str(path),
            f"unit_projection 的 {detail} 不能由 Survey 与 Rules manifest 重建",
            "使用同一次 scan/finalize 的 coverage.json 与 rules-manifest.json",
        )

    generic_candidates: set[str] = set()
    dispositions = coverage.get("dispositions")
    if not isinstance(dispositions, list):
        fail(str(path), "coverage 缺少 dispositions", "重新运行 finalize")
    group_members: dict[str, list[str]] = {}
    for item in locations:
        group_id = item.get("review_group_id")
        candidate_id = item.get("candidate_id")
        if isinstance(group_id, str) and isinstance(candidate_id, str):
            group_members.setdefault(group_id, []).append(candidate_id)
    resolved_review_candidates: list[str] = []
    rules_dispositions: dict[str, list[str]] = {}
    generic_placements: dict[str, tuple[str, str]] = {}
    generic_plans: list[dict[str, JsonValue]] = []
    for number, raw in enumerate(dispositions, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"dispositions 第 {number} 项不是 object", "重新运行 finalize")
        owner = raw.get("owner")
        allowed_fields = {
            "rules": {"target", "owner", "candidate_ids"},
            "generic": {"target", "owner", "candidate_ids", "evidence"},
            "exclude": {"target", "owner", "candidate_ids", "reason", "evidence"},
        }.get(owner if isinstance(owner, str) else "")
        if allowed_fields is None:
            fail(
                str(path),
                f"dispositions 第 {number} 项 owner 不属于 producer 闭集",
                "重新运行 finalize；这里只接受 rules、generic 或 exclude",
            )
        validate_object_keys(raw, f"{path} dispositions 第 {number} 项", allowed_fields)
        target = raw.get("target")
        candidates = raw.get("candidate_ids")
        if (
            not isinstance(target, str)
            or not isinstance(candidates, list)
            or not candidates
            or any(not isinstance(value, str) or value not in locations_by_id for value in candidates)
            or len(candidates) != len(set(cast(list[str], candidates)))
        ):
            fail(str(path), f"dispositions 第 {number} 项身份或候选无效", "重新运行 finalize")
        typed_candidates = cast(list[str], candidates)
        if target.startswith("candidate:"):
            expected_candidates = [target.removeprefix("candidate:")]
        elif target.startswith("group:"):
            expected_candidates = group_members.get(target.removeprefix("group:"))
        else:
            expected_candidates = None
        if expected_candidates is None or typed_candidates != expected_candidates:
            fail(str(path), f"dispositions 第 {number} 项 target 与候选不一致", "重新运行 finalize")
        if owner == "generic":
            evidence = raw.get("evidence")
            if not isinstance(evidence, dict):
                fail(str(path), f"dispositions 第 {number} 项缺少 Generic 证据", "重新运行 finalize")
            normalized_evidence = validate_generic_evidence(
                evidence,
                typed_candidates,
                f"{path} dispositions 第 {number} 项 evidence",
            )
            validate_generic_group_placement(
                normalized_evidence,
                locations_by_id,
                generic_placements,
                f"{path} dispositions 第 {number} 项 evidence",
            )
            generic_plans.append(
                {
                    "target": target,
                    "candidate_ids": list(typed_candidates),
                    "locations": [locations_by_id[candidate_id] for candidate_id in typed_candidates],
                    "evidence": normalized_evidence,
                }
            )
            overlap = generic_candidates.intersection(typed_candidates)
            if overlap:
                fail(str(path), f"Generic 候选 {min(overlap)} 被重复归属", "重新运行 finalize")
            generic_candidates.update(typed_candidates)
        elif owner == "rules":
            if target in rules_dispositions:
                fail(str(path), f"Rules target {target} 被重复决定", "重新运行 survey finalize")
            rules_dispositions[target] = typed_candidates
        elif owner == "exclude":
            if any(
                not isinstance(raw.get(field), str) or not cast(str, raw[field]).strip()
                for field in ("reason", "evidence")
            ):
                fail(str(path), f"dispositions 第 {number} 项排除证据无效", "重新运行 finalize")
        resolved_review_candidates.extend(typed_candidates)
    if counts.get("generic_groups") != len(generic_placements):
        fail(str(path), "coverage generic_groups 与结构化映射不一致", "重新运行 survey finalize")
    manifest_targets: set[str] = set()
    covered_by_target: dict[str, set[str]] = {target: set() for target in rules_dispositions}
    for item in rules_manifest:
        rule_number = cast(int, item["rule_number"])
        targets = cast(list[str], item["targets"])
        candidate_ids = cast(list[str], item["candidate_ids"])
        allowed_candidates: set[str] = set()
        for target in targets:
            manifest_targets.add(target)
            disposition_candidates = rules_dispositions.get(target)
            if disposition_candidates is None:
                fail(
                    "rules-manifest.json",
                    f"第 {rule_number} 条引用了非 Rules disposition {target}",
                    "使用同一次 finalize 的 coverage 与 Rules manifest",
                )
            allowed_candidates.update(disposition_candidates)
            covered_by_target[target].update(set(candidate_ids).intersection(disposition_candidates))
        if not set(candidate_ids) <= allowed_candidates:
            fail(
                "rules-manifest.json",
                f"第 {rule_number} 条候选与 Rules dispositions 不一致",
                "重新运行 survey finalize",
            )
    incomplete_targets = sorted(
        target
        for target, candidates in rules_dispositions.items()
        if covered_by_target.get(target) != set(candidates)
    )
    if manifest_targets != set(rules_dispositions) or incomplete_targets:
        missing = sorted(set(rules_dispositions) - manifest_targets)
        fail(
            "rules-manifest.json",
            f"Rules disposition {(missing or incomplete_targets or ['未知 target'])[0]} 没有完整真实 Rule recipe",
            "重新运行 survey finalize",
        )
    unresolved = coverage.get("unresolved")
    missing_targets = coverage.get("missing_targets")
    if not isinstance(unresolved, list) or not isinstance(missing_targets, list):
        fail(str(path), "coverage 缺少 unresolved 或 missing_targets", "重新运行 finalize")
    if counts.get("unresolved") != len(unresolved):
        fail(str(path), "coverage unresolved 计数不一致", "重新运行 finalize")
    for number, raw in enumerate(unresolved, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"unresolved 第 {number} 项不是 object", "重新运行 finalize")
        target = raw.get("target")
        if not isinstance(target, str):
            fail(str(path), f"unresolved 第 {number} 项缺少自然 target", "重新运行 finalize")
        if target.startswith("candidate:"):
            candidate_id = target.removeprefix("candidate:")
            if candidate_id not in locations_by_id:
                fail(str(path), f"unresolved 第 {number} 项引用未知候选", "重新运行 finalize")
            target_candidates = [candidate_id]
        elif target.startswith("group:"):
            group_id = target.removeprefix("group:")
            target_candidates = group_members.get(group_id)
            if target_candidates is None:
                fail(str(path), f"unresolved 第 {number} 项引用未知关系组", "重新运行 finalize")
        else:
            fail(str(path), f"unresolved 第 {number} 项 target 无效", "重新运行 finalize")
        candidates = raw.get("candidate_ids")
        if candidates is not None and (
            not isinstance(candidates, list)
            or not candidates
            or any(not isinstance(value, str) or value not in target_candidates for value in candidates)
            or len(candidates) != len(set(cast(list[str], candidates)))
        ):
            fail(str(path), f"unresolved 第 {number} 项候选无效", "重新运行 finalize")
        resolved_review_candidates.extend(target_candidates)
    review_candidates = {
        cast(str, item["candidate_id"])
        for item in locations
        if item.get("classification") == "review" and isinstance(item.get("candidate_id"), str)
    }
    if (
        len(resolved_review_candidates) != len(set(resolved_review_candidates))
        or set(resolved_review_candidates) != review_candidates
    ):
        fail(
            str(path),
            "coverage 没有逐项覆盖 survey 的全部 Review 候选",
            "使用同一次 survey finalize 生成的 coverage.json",
        )
    if any(not isinstance(value, str) for value in missing_targets):
        fail(str(path), "missing_targets 结构无效", "重新运行 finalize")
    complete_value = coverage.get("complete")
    expected_complete = not unresolved and not missing_targets
    if not isinstance(complete_value, bool) or complete_value != expected_complete:
        fail(str(path), "coverage complete 与未解决事实矛盾", "重新运行 finalize")
    return projection, generic_candidates, complete_value, generic_plans
