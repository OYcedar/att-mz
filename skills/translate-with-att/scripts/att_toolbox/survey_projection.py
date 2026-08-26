"""从 Survey 与 Rules manifest 独立重建 ATT Unit 投影。"""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

from att_skill_tools import JsonValue, fail, read_json_object, validate_object_keys

from .rpg_control_codes import PLAIN_TEXT, is_structural_blank


def read_rules_manifest(path: Path) -> list[dict[str, JsonValue]]:
    root = read_json_object(path, "rules-manifest.json")
    validate_object_keys(root, str(path), {"rules"})
    raw_rules = root.get("rules")
    if not isinstance(raw_rules, list):
        fail(str(path), "rules 不是 array", "使用同一次 survey finalize 生成的 rules-manifest.json")
    result: list[dict[str, JsonValue]] = []
    for number, raw in enumerate(raw_rules, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"第 {number} 项 Rules manifest 不是 object", "重新运行 survey finalize")
        item = dict(raw)
        validate_object_keys(
            item,
            f"{path} 第 {number} 项",
            {"rule_number", "rule", "candidate_ids", "locations", "expected_manual_ids", "targets"},
        )
        rule_number = item.get("rule_number")
        if not isinstance(rule_number, int) or isinstance(rule_number, bool) or rule_number != number:
            fail(str(path), f"第 {number} 项缺少连续自然 rule_number", "重新运行 survey finalize")
        for field in ("candidate_ids", "locations", "expected_manual_ids", "targets"):
            values = item.get(field)
            if (
                not isinstance(values, list)
                or any(not isinstance(value, str) or not value for value in values)
                or len(values) != len(set(cast(list[str], values)))
            ):
                fail(str(path), f"第 {number} 项 {field} 无效或重复", "重新运行 survey finalize")
        if not isinstance(item.get("rule"), dict) or not cast(list[JsonValue], item["candidate_ids"]):
            fail(str(path), f"第 {number} 项缺少 rule 或候选", "重新运行 survey finalize")
        result.append(item)
    return result


def _projection_row(
    manual_id: str,
    source_text: str,
    manual_type: str,
    control_contract: Mapping[str, JsonValue],
    location: Mapping[str, JsonValue],
    *,
    owner: str,
    rule_number: int | None = None,
) -> dict[str, JsonValue]:
    row: dict[str, JsonValue] = {
        "manual_id": manual_id,
        "source_text": source_text,
        "manual_type": manual_type,
        "control_contract": dict(control_contract),
        "source": cast(str, location["source"]),
        "candidate_id": cast(str, location["candidate_id"]),
        "owner": owner,
    }
    review_group_id = location.get("review_group_id")
    if isinstance(review_group_id, str):
        row["review_group_id"] = review_group_id
    if rule_number is not None:
        row["rule_number"] = rule_number
    return row


def project_builtin_units(
    locations: Sequence[Mapping[str, JsonValue]],
) -> list[dict[str, JsonValue]]:
    projection: list[dict[str, JsonValue]] = []
    for item in locations:
        if item.get("classification") != "builtin":
            continue
        manual_id = item.get("expected_manual_id")
        if not isinstance(manual_id, str):
            continue
        source_text = item.get("source_text")
        manual_type = item.get("manual_type")
        control_contract = item.get("control_contract")
        source = item.get("source")
        candidate_id = item.get("candidate_id")
        if (
            not isinstance(source_text, str)
            or manual_type not in {"fixed", "free"}
            or not isinstance(control_contract, dict)
            or not isinstance(source, str)
            or not isinstance(candidate_id, str)
        ):
            fail("locations.jsonl", f"{manual_id} 缺少 Builtin 投影事实", "使用当前工具重新执行 scan")
        projection.append(
            _projection_row(
                manual_id,
                source_text,
                cast(str, manual_type),
                control_contract,
                item,
                owner="builtin",
            )
        )
    return projection


def _python_extract_pattern(pattern: str) -> re.Pattern[str]:
    candidate = re.sub(r"\(\?<([A-Za-z_][A-Za-z0-9_]*)>", r"(?P<\1>", pattern).replace(r"\z", r"\Z")
    try:
        return re.compile(candidate)
    except re.error as error:
        fail("rules-manifest.json", f"Rules pattern 无法由调查器投影：{error}", "重新运行 scan/finalize")


def _rule_control_contract(location: Mapping[str, JsonValue]) -> dict[str, JsonValue]:
    explicit = location.get("control_contract")
    return dict(explicit) if isinstance(explicit, dict) else PLAIN_TEXT.json()


def project_rule_units(
    manifest: Sequence[Mapping[str, JsonValue]],
    locations_by_id: Mapping[str, Mapping[str, JsonValue]],
    *,
    validate_expected_manual_ids: bool = True,
) -> list[dict[str, JsonValue]]:
    projection: list[dict[str, JsonValue]] = []
    seen_candidates: set[str] = set()
    for manifest_item in manifest:
        rule = cast(dict[str, JsonValue], manifest_item["rule"])
        raw_candidate_ids = cast(list[JsonValue], manifest_item["candidate_ids"])
        rule_number = cast(int, manifest_item["rule_number"])
        pattern = rule.get("pattern")
        compiled = _python_extract_pattern(pattern) if isinstance(pattern, str) else None
        item_ids: list[str] = []
        item_locations: list[str] = []
        for raw_candidate_id in raw_candidate_ids:
            if not isinstance(raw_candidate_id, str) or raw_candidate_id not in locations_by_id:
                fail("rules-manifest.json", "Rules manifest 引用了未知位置", "重新运行 scan/finalize")
            if raw_candidate_id in seen_candidates:
                fail("rules-manifest.json", f"候选 {raw_candidate_id} 被多个 Rule 消费", "重新运行 finalize")
            seen_candidates.add(raw_candidate_id)
            item_ids.append(raw_candidate_id)
            location = locations_by_id[raw_candidate_id]
            if location.get("rule") != rule:
                fail(
                    "rules-manifest.json",
                    f"第 {rule_number} 条 Rule 与 {raw_candidate_id} 的 Survey 方案不一致",
                    "使用同一次 scan/finalize 的 Survey 与 Rules manifest",
                )
            natural_location = location.get("location")
            manual_id = location.get("expected_manual_id")
            source_text = location.get("source_text")
            manual_type = location.get("manual_type")
            if (
                not isinstance(natural_location, str)
                or not isinstance(manual_id, str)
                or not isinstance(source_text, str)
                or manual_type not in {"fixed", "free"}
            ):
                fail("locations.jsonl", f"{raw_candidate_id} 缺少 Rules 投影事实", "重新运行 scan")
            item_locations.append(natural_location)
            if compiled is None:
                projected_texts = [source_text]
            else:
                if "text" not in compiled.groupindex:
                    fail(
                        "rules-manifest.json",
                        f"第 {rule_number} 条 pattern 缺少 text 捕获",
                        "重新运行 finalize",
                    )
                projected_texts = [
                    match.group("text")
                    for match in compiled.finditer(source_text)
                    if match.group("text") is not None and not is_structural_blank(match.group("text"))
                ]
                if not projected_texts:
                    fail(
                        "rules-manifest.json",
                        f"第 {rule_number} 条不能投影 {manual_id}",
                        "来源或规则已不一致；重新运行 scan/finalize",
                    )
            contract = _rule_control_contract(location)
            for unit_index, projected_text in enumerate(projected_texts):
                projected_id = re.sub(r"text\[0\]\Z", f"text[{unit_index}]", manual_id)
                if projected_id == manual_id and unit_index != 0:
                    fail("locations.jsonl", f"{manual_id} 不能表达多个 Rules Unit", "重新运行 scan")
                projection.append(
                    _projection_row(
                        projected_id,
                        projected_text,
                        cast(str, manual_type),
                        contract,
                        location,
                        owner="rules",
                        rule_number=rule_number,
                    )
                )
        if item_ids != cast(list[str], manifest_item["candidate_ids"]):
            raise AssertionError("Rules candidate 类型校验没有保持顺序")
        if item_locations != cast(list[str], manifest_item["locations"]):
            fail(
                "rules-manifest.json",
                f"第 {rule_number} 条 locations 与 Survey 不一致",
                "使用同一次 scan/finalize 的 Rules manifest",
            )
        expected_ids = sorted(
            cast(str, item["manual_id"]) for item in projection if item.get("rule_number") == rule_number
        )
        if validate_expected_manual_ids and expected_ids != cast(
            list[str], manifest_item["expected_manual_ids"]
        ):
            fail(
                "rules-manifest.json",
                f"第 {rule_number} 条 expected_manual_ids 与真实投影不一致",
                "重新运行 survey finalize",
            )
    return projection
