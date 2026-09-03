"""读取 ATT Translation export，并投影 WriteBack 实际会使用的文本。"""

from __future__ import annotations

import hashlib
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

from att_skill_tools import (
    JsonValue,
    fail,
    parse_json_text,
    physical_jsonl_lines,
    read_physical_text,
    require_file,
    validate_object_keys,
)

EXPORT_FIELDS = {
    "manual_id",
    "source",
    "translation",
    "state",
    "origin",
    "type",
    "owner",
    "rule_number",
    "rejected_candidate_json",
}


def read_translation_export(path: Path) -> list[dict[str, JsonValue]]:
    """按当前 CLI 契约读取完整导出；任何不完整身份都不能充当下游证据。"""

    source = require_file(path, "ATT Translation export JSONL")
    rows: list[dict[str, JsonValue]] = []
    seen: set[str] = set()
    for line_number, line in physical_jsonl_lines(read_physical_text(source), str(source)):
        if not line.strip():
            fail(str(source), f"第 {line_number} 行为空", "重新执行 ATT translation export")
        raw = parse_json_text(line, f"{source} 第 {line_number} 行")
        if not isinstance(raw, dict):
            fail(str(source), f"第 {line_number} 行不是 object", "重新执行 ATT translation export")
        row = dict(raw)
        validate_object_keys(row, f"{source} 第 {line_number} 行", EXPORT_FIELDS)
        required = {"manual_id", "source", "translation", "state", "origin", "type"}
        missing = sorted(required - set(row))
        if missing:
            fail(
                str(source),
                f"第 {line_number} 行缺少字段 {', '.join(missing)}",
                "重新执行 ATT translation export",
            )
        manual_id = row.get("manual_id")
        source_lines = row.get("source")
        state = row.get("state")
        unit_type = row.get("type")
        origin = row.get("origin")
        translation = row.get("translation")
        has_rejected_candidate = "rejected_candidate_json" in row
        rejected_candidate = row.get("rejected_candidate_json")
        if not isinstance(manual_id, str) or not manual_id or manual_id in seen:
            fail(str(source), f"第 {line_number} 行 manual_id 无效或重复", "重新执行 ATT translation export")
        if not isinstance(source_lines, list) or any(not isinstance(value, str) for value in source_lines):
            fail(str(source), f"{manual_id} 的 source 不是 string array", "重新执行 ATT translation export")
        if (
            not isinstance(state, str)
            or state not in {"current", "pending", "rejected"}
            or not isinstance(unit_type, str)
            or unit_type not in {"fixed", "free"}
        ):
            fail(str(source), f"{manual_id} 的 state 或 type 无效", "重新执行 ATT translation export")
        if not isinstance(origin, str) or origin not in {"none", "automatic", "manual"}:
            fail(str(source), f"{manual_id} 的 origin 无效", "重新执行 ATT translation export")
        owner = row.get("owner")
        rule_number = row.get("rule_number")
        if owner is not None and (not isinstance(owner, str) or owner not in {"builtin", "rules"}):
            fail(str(source), f"{manual_id} 的 owner 无效", "重新执行 ATT translation export")
        if owner == "rules":
            if not isinstance(rule_number, int) or isinstance(rule_number, bool) or rule_number <= 0:
                fail(str(source), f"{manual_id} 缺少自然 rule_number", "重新执行 ATT translation export")
        elif rule_number is not None:
            fail(str(source), f"{manual_id} 不应包含 rule_number", "重新执行 ATT translation export")
        if state == "current":
            if not isinstance(translation, list) or any(not isinstance(value, str) for value in translation):
                fail(str(source), f"{manual_id} 的 current translation 不是 string array", "重新导出当前项目")
            if origin not in {"automatic", "manual"}:
                fail(str(source), f"{manual_id} 的 current origin 无效", "重新导出当前项目")
            if has_rejected_candidate:
                fail(str(source), f"{manual_id} 不应包含 rejected_candidate_json", "重新导出当前项目")
        elif state == "pending":
            if translation is not None:
                fail(str(source), f"{manual_id} 的 pending translation 必须为 null", "重新导出当前项目")
            if origin != "none":
                fail(str(source), f"{manual_id} 的 pending origin 必须为 none", "重新导出当前项目")
            if has_rejected_candidate:
                fail(str(source), f"{manual_id} 不应包含 rejected_candidate_json", "重新导出当前项目")
        else:
            if translation is not None:
                fail(str(source), f"{manual_id} 的 rejected translation 必须为 null", "重新导出当前项目")
            if origin not in {"automatic", "manual"}:
                fail(str(source), f"{manual_id} 的 rejected origin 无效", "重新导出当前项目")
            if not isinstance(rejected_candidate, str) or not rejected_candidate:
                fail(str(source), f"{manual_id} 缺少 rejected_candidate_json", "重新导出当前项目")
        seen.add(manual_id)
        rows.append(row)
    return rows


def projected_write_back_text(rows: list[dict[str, JsonValue]]) -> tuple[str, int]:
    """投影当前 WriteBack 正文；未接受条目仍会写回原文。"""

    values: list[str] = []
    translated = 0
    for row in rows:
        if row["state"] == "current":
            lines = cast(list[object], row["translation"])
            translated += 1
        else:
            lines = cast(list[object], row["source"])
        values.extend(cast(str, value) for value in lines)
    return "".join(values), translated


def translation_export_identity(path: Path, rows: Sequence[Mapping[str, JsonValue]]) -> dict[str, JsonValue]:
    """保存人类能够核对的输入身份，不把摘要当作业务对象标识。"""

    source = require_file(path, "ATT Translation export JSONL")
    body = source.read_bytes()
    return {
        "path": str(source.resolve()),
        "bytes": len(body),
        "sha256": hashlib.sha256(body).hexdigest(),
        "unit_count": len(rows),
        "current_translation_count": sum(row.get("state") == "current" for row in rows),
    }
