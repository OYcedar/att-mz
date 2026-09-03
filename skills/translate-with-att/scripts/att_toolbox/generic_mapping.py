"""校验 Survey Generic 决定中的语义分组与外部映射证据。"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from typing import cast

from att_skill_tools import JsonValue, fail, validate_object_keys

from .survey_sources import GENERIC_EVIDENCE_FIELDS

_MAPPING_FIELD = "extract_group_unit_write_back_mapping"


def generic_recipe(location: Mapping[str, JsonValue]) -> dict[str, JsonValue] | None:
    """把一个已调查位置规范化为可精确反向定位的 Generic recipe。"""

    kind = location.get("generic_kind")
    locator = location.get("generic_locator")
    physical_file = location.get("physical_file")
    source_text = location.get("source_text")
    candidate_id = location.get("candidate_id")
    if (
        not isinstance(kind, str)
        or kind not in {"javascript_literal", "plain_text_line", "json_string"}
        or not isinstance(locator, dict)
        or not isinstance(physical_file, str)
        or not isinstance(source_text, str)
        or not isinstance(candidate_id, str)
        or "\r" in source_text
        or "\0" in source_text
    ):
        return None
    recipe: dict[str, JsonValue] = {
        "candidate_id": candidate_id,
        "physical_file": physical_file,
        "source_kind": kind,
        "source": source_text,
    }
    if kind == "json_string":
        path = locator.get("path")
        decode_positions = locator.get("decode_positions")
        if (
            not isinstance(path, list)
            or any(not isinstance(value, (str, int)) or isinstance(value, bool) for value in path)
            or not isinstance(decode_positions, list)
            or any(
                not isinstance(value, int) or isinstance(value, bool) or value < 0
                for value in decode_positions
            )
        ):
            return None
        recipe.update({"path": list(path), "decode_positions": list(decode_positions)})
        return recipe
    line = locator.get("line")
    if not isinstance(line, int) or isinstance(line, bool) or line <= 0:
        return None
    recipe["source_line"] = line
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
            or not isinstance(quote, str)
            or quote not in {"'", '"'}
        ):
            return None
        recipe.update({"start": start, "end": end, "quote": quote})
    return recipe


def validate_generic_evidence(
    value: Mapping[str, JsonValue],
    candidate_ids: Sequence[str],
    object_name: str,
) -> dict[str, JsonValue]:
    """返回完整当前证据，并证明语义 Group 精确覆盖所选候选。"""

    evidence = dict(value)
    validate_object_keys(evidence, object_name, set(GENERIC_EVIDENCE_FIELDS))
    for field in GENERIC_EVIDENCE_FIELDS:
        if field == _MAPPING_FIELD:
            continue
        item = evidence.get(field)
        if not isinstance(item, str) or not item.strip():
            fail(object_name, f"{field} 不是非空证据", "填写该 Generic 所有权证据后重新 finalize")

    mapping = evidence.get(_MAPPING_FIELD)
    if not isinstance(mapping, dict):
        fail(object_name, f"{_MAPPING_FIELD} 不是 object", "按当前 groups 结构填写精确映射")
    validate_object_keys(mapping, f"{object_name}.{_MAPPING_FIELD}", {"groups"})
    raw_groups = mapping.get("groups")
    if not isinstance(raw_groups, list) or not raw_groups:
        fail(object_name, "Generic 映射缺少非空 groups", "为每个可共同理解的语义组填写一项")

    group_ids: set[str] = set()
    flattened: list[str] = []
    normalized_groups: list[dict[str, JsonValue]] = []
    for number, raw_group in enumerate(raw_groups, start=1):
        if not isinstance(raw_group, dict):
            fail(object_name, f"Generic groups 第 {number} 项不是 object", "修正当前决定的映射结构")
        validate_object_keys(
            raw_group, f"{object_name}.groups 第 {number} 项", {"id", "kind", "candidate_ids"}
        )
        group_id = raw_group.get("id")
        kind = raw_group.get("kind")
        raw_candidates = raw_group.get("candidate_ids")
        if (
            not isinstance(group_id, str)
            or not group_id.strip()
            or group_id in group_ids
            or not isinstance(kind, str)
            or not kind.strip()
            or not isinstance(raw_candidates, list)
            or not raw_candidates
            or any(not isinstance(item, str) or not item for item in raw_candidates)
            or len(raw_candidates) != len(set(cast(list[str], raw_candidates)))
        ):
            fail(object_name, f"Generic groups 第 {number} 项身份、kind 或候选无效", "修正当前决定的映射结构")
        typed_candidates = cast(list[str], raw_candidates)
        group_ids.add(group_id)
        flattened.extend(typed_candidates)
        normalized_groups.append({"id": group_id, "kind": kind, "candidate_ids": list(typed_candidates)})

    if flattened != list(candidate_ids):
        fail(
            object_name,
            "Generic groups 没有按自然顺序逐项覆盖当前决定候选",
            "让 candidate_ids 与当前 target 的候选列表精确一致且每项只出现一次",
        )
    evidence[_MAPPING_FIELD] = {"groups": normalized_groups}
    return evidence


def generic_mapping_groups(evidence: Mapping[str, JsonValue]) -> list[dict[str, JsonValue]]:
    """读取已经通过 validate_generic_evidence 的规范语义组。"""

    mapping = cast(dict[str, JsonValue], evidence[_MAPPING_FIELD])
    return cast(list[dict[str, JsonValue]], mapping["groups"])


def validate_generic_group_placement(
    evidence: Mapping[str, JsonValue],
    locations_by_id: Mapping[str, Mapping[str, JsonValue]],
    seen_group_ids: set[str],
    object_name: str,
) -> None:
    """证明 Group ID 全局唯一，且每组只落入一个物理 Semantic Scope。"""

    for group in generic_mapping_groups(evidence):
        group_id = cast(str, group["id"])
        if group_id in seen_group_ids:
            fail(object_name, f"Generic Group ID {group_id} 在项目中重复", "为每个语义组使用全局唯一自然 ID")
        candidate_ids = cast(list[str], group["candidate_ids"])
        physical_values = [
            locations_by_id[candidate_id].get("physical_file")
            for candidate_id in candidate_ids
            if candidate_id in locations_by_id
        ]
        if (
            len(physical_values) != len(candidate_ids)
            or any(not isinstance(value, str) for value in physical_values)
            or len(set(cast(list[str], physical_values))) != 1
        ):
            fail(
                object_name,
                f"Generic Group {group_id} 跨越多个物理输入文件或引用未知候选",
                "按物理文件拆分 groups，同时用 kind 保留共同语境",
            )
        seen_group_ids.add(group_id)


def _json_lines(values: Sequence[Mapping[str, JsonValue]]) -> str:
    return "".join(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n" for value in values)


def generic_materials(
    plans: Sequence[Mapping[str, JsonValue]],
) -> tuple[dict[str, str], dict[str, JsonValue]]:
    groups_by_source: dict[str, list[dict[str, object]]] = {}
    seen_group_ids: set[str] = set()
    decisions: list[dict[str, JsonValue]] = []
    target_by_candidate: dict[str, str] = {}
    for plan in plans:
        target = cast(str, plan["target"])
        raw_locations = cast(list[JsonValue], plan["locations"])
        locations = [value for value in raw_locations if isinstance(value, dict)]
        locations_by_candidate = {
            cast(str, location["candidate_id"]): location
            for location in locations
            if isinstance(location.get("candidate_id"), str)
        }
        recipes_by_candidate: dict[str, dict[str, JsonValue]] = {}
        for location in locations:
            recipe = generic_recipe(location)
            if recipe is None:
                raise AssertionError("Generic 计划包含未验收的往返类型")
            candidate_id = cast(str, recipe["candidate_id"])
            recipes_by_candidate[candidate_id] = recipe
            target_by_candidate[candidate_id] = target
        validate_generic_group_placement(
            cast(dict[str, JsonValue], plan["evidence"]),
            locations_by_candidate,
            seen_group_ids,
            target,
        )
        raw_groups = generic_mapping_groups(cast(dict[str, JsonValue], plan["evidence"]))
        for raw_group in raw_groups:
            candidate_ids = cast(list[str], raw_group["candidate_ids"])
            source_recipes = [recipes_by_candidate[candidate_id] for candidate_id in candidate_ids]
            physical_files = {cast(str, recipe["physical_file"]) for recipe in source_recipes}
            if len(physical_files) != 1:
                raise AssertionError("已验收的 Generic Group 物理范围发生分叉")
            physical_file = physical_files.pop()
            group_id = cast(str, raw_group["id"])
            groups_by_source.setdefault(physical_file, []).append(
                {
                    "id": group_id,
                    "kind": cast(str, raw_group["kind"]),
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
                        "group_kind": kind,
                        "group_line": group_line,
                        "unit_id": unit_id,
                        "unit_number": unit_number,
                        "manual_id": f"{input_relative}:line{group_line}:unit{unit_number}:text",
                    }
                )
            unit_count += len(units)
            serialized_groups.append({"id": group_id, "kind": kind, "units": units})
        files[input_file] = _json_lines(serialized_groups)
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
