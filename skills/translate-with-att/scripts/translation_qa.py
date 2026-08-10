#!/usr/bin/env python3
"""从 ATT Translation export 和调查事实生成非阻断译后 QA。"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import time
import tomllib
import unicodedata
from collections import Counter
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    atomic_write_directory,
    atomic_write_text,
    display_path,
    fail,
    parse_json_text,
    protect_outputs,
    read_json_object,
    require_directory,
    require_file,
    run_cli,
    validate_object_keys,
)
from att_toolbox.rpg_control_codes import is_structural_blank
from att_toolbox.survey import load_survey, verify_source_baseline

_CONTROL_SHAPE = re.compile(
    r"(?:\\|\x1b)(?:[A-Za-z]+(?:\[[^\]\r\n]*\]|<[^>\r\n]*>)?|[\\{}$.|!><^])|"
    r"\{\{[^{}\r\n]+\}\}|\$\{[^{}\r\n]+\}|%[A-Za-z_][A-Za-z0-9_]*%|%[0-9]+|"
    r"</?[A-Za-z][^>\r\n]*>|\f"
)
_SOURCE_WORD = re.compile(r"(?<![A-Za-z])[A-Za-z][A-Za-z'-]{1,}(?![A-Za-z])")
_EXPORT_FIELDS = {
    "manual_id",
    "source",
    "translation",
    "state",
    "origin",
    "type",
    "owner",
    "rule_number",
}


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="扫描 ATT 当前译文、调查事实及可选运行证据，生成非阻断 QA。")
    subparsers = parser.add_subparsers(dest="command", required=True)
    scan = subparsers.add_parser("scan", help="扫描当前 Translation export")
    scan.add_argument("--translations", type=Path, required=True, help="ATT translation export JSONL")
    scan.add_argument("--survey", type=Path, required=True, help="同一来源快照的 survey 作业目录")
    scan.add_argument("--generic-manifest", type=Path, help="finalize 生成的可选 Generic 精确映射")
    scan.add_argument("--terminology", type=Path, help="本次 Translate 使用的 terminology.toml")
    scan.add_argument("--write-back-preview", type=Path, help="可选 WriteBack 预览或验证报告 JSON")
    scan.add_argument("--runtime-report", type=Path, help="可选 NW.js smoke/observe report.json")
    scan.add_argument("--output", type=Path, required=True, help="QA 作业目录")
    scan.add_argument("--replace", action="store_true")
    manual = subparsers.add_parser("manual", help="只输出供 att manual export --ids 使用的自然 ID JSONL")
    manual.add_argument("--scan", type=Path, required=True, help="scan 生成的 QA 作业目录")
    manual.add_argument("--output", type=Path, required=True, help="自然 ID JSONL")
    manual.add_argument("--replace", action="store_true")
    return parser


def _json_text(value: JsonValue) -> str:
    return json.dumps(value, ensure_ascii=False, indent=2) + "\n"


def _json_lines(values: Sequence[Mapping[str, JsonValue]]) -> str:
    return "".join(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n" for value in values)


def _file_fact(path: Path, description: str) -> dict[str, JsonValue]:
    source = require_file(path, description)
    raw = source.read_bytes()
    return {"path": str(source.resolve()), "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}


def _read_translation_export(path: Path) -> list[dict[str, JsonValue]]:
    source = require_file(path, "ATT Translation export JSONL")
    rows: list[dict[str, JsonValue]] = []
    seen: set[str] = set()
    for line_number, line in enumerate(source.read_text(encoding="utf-8-sig").splitlines(), start=1):
        if not line.strip():
            fail(str(source), f"第 {line_number} 行为空", "重新执行 ATT translation export")
        raw = parse_json_text(line, f"{source} 第 {line_number} 行")
        if not isinstance(raw, dict):
            fail(str(source), f"第 {line_number} 行不是 object", "重新执行 ATT translation export")
        row = dict(raw)
        validate_object_keys(row, f"{source} 第 {line_number} 行", _EXPORT_FIELDS)
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
        kind = row.get("type")
        origin = row.get("origin")
        translation = row.get("translation")
        if not isinstance(manual_id, str) or not manual_id or manual_id in seen:
            fail(str(source), f"第 {line_number} 行 manual_id 无效或重复", "重新执行 ATT translation export")
        if not isinstance(source_lines, list) or any(not isinstance(value, str) for value in source_lines):
            fail(str(source), f"{manual_id} 的 source 不是 string array", "重新执行 ATT translation export")
        if state not in {"current", "pending", "rejected"} or kind not in {"fixed", "free"}:
            fail(str(source), f"{manual_id} 的 state 或 type 无效", "重新执行 ATT translation export")
        if not isinstance(origin, str):
            fail(str(source), f"{manual_id} 的 origin 无效", "重新执行 ATT translation export")
        owner = row.get("owner")
        rule_number = row.get("rule_number")
        if owner is not None and owner not in {"builtin", "rules"}:
            fail(str(source), f"{manual_id} 的 owner 无效", "重新执行 ATT translation export")
        if owner == "rules":
            if not isinstance(rule_number, int) or isinstance(rule_number, bool) or rule_number <= 0:
                fail(str(source), f"{manual_id} 缺少自然 rule_number", "重新执行 ATT translation export")
        elif rule_number is not None:
            fail(str(source), f"{manual_id} 不应包含 rule_number", "重新执行 ATT translation export")
        if state == "current":
            if not isinstance(translation, list) or any(not isinstance(value, str) for value in translation):
                fail(str(source), f"{manual_id} 的 current translation 不是 string array", "重新导出当前项目")
        elif state == "pending" and translation is not None:
            fail(str(source), f"{manual_id} 的 pending translation 必须为 null", "重新导出当前项目")
        elif state == "rejected" and translation is None:
            fail(str(source), f"{manual_id} 缺少 Rejected candidate", "重新导出当前项目")
        seen.add(manual_id)
        rows.append(row)
    return rows


def _generic_manifest(path: Path | None) -> dict[str, tuple[str, str]] | None:
    if path is None:
        return None
    source = require_file(path, "Generic 精确映射 manifest")
    root = read_json_object(source, "Generic 精确映射 manifest")
    validate_object_keys(root, str(source), {"sources", "decisions", "recipes"})
    recipes = root.get("recipes")
    if not isinstance(recipes, list):
        fail(str(source), "recipes 不是 array", "重新运行 rpg_maker_survey.py finalize")
    output: dict[str, tuple[str, str]] = {}
    seen_candidates: set[str] = set()
    for number, raw in enumerate(recipes, start=1):
        if not isinstance(raw, dict):
            fail(str(source), f"第 {number} 项 recipe 不是 object", "重新运行 finalize")
        manual_id = raw.get("manual_id")
        candidate_id = raw.get("candidate_id")
        original = raw.get("source")
        if (
            not isinstance(manual_id, str)
            or not manual_id
            or manual_id in output
            or not isinstance(candidate_id, str)
            or not candidate_id
            or candidate_id in seen_candidates
            or not isinstance(original, str)
        ):
            fail(str(source), f"第 {number} 项 recipe 身份或原文无效", "重新运行 finalize")
        output[manual_id] = (candidate_id, original)
        seen_candidates.add(candidate_id)
    return output


def _terminology(path: Path | None) -> list[tuple[str, str]]:
    if path is None:
        return []
    source = require_file(path, "ATT terminology.toml")
    try:
        root = tomllib.loads(source.read_text(encoding="utf-8-sig"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        fail(str(source), f"术语 TOML 无法读取：{error}", "使用 ATT 当前 terminology 语法修正文件")
    raw_terms = root.get("term")
    if not isinstance(raw_terms, list):
        fail(str(source), "term 必须是 array", "使用 ATT 当前 terminology 语法修正文件")
    output: list[tuple[str, str]] = []
    for number, raw_value in enumerate(cast(list[object], raw_terms), start=1):
        if not isinstance(raw_value, dict):
            fail(str(source), f"第 {number} 项 term 不是 object", "使用 ATT 当前 terminology 语法修正文件")
        raw = cast(dict[str, object], raw_value)
        term = raw.get("term")
        translation = raw.get("translation")
        triggers = raw.get("triggers", [term])
        if not isinstance(term, str) or not isinstance(translation, str) or not isinstance(triggers, list):
            fail(str(source), f"第 {number} 项术语字段无效", "先让 ATT 解析并确认当前术语文件")
        raw_triggers = cast(list[object], triggers)
        typed_triggers = [trigger for trigger in raw_triggers if isinstance(trigger, str)]
        if len(typed_triggers) != len(raw_triggers):
            fail(str(source), f"第 {number} 项术语字段无效", "先让 ATT 解析并确认当前术语文件")
        output.extend((trigger, translation) for trigger in typed_triggers)
    return output


def _display_width(value: str) -> int:
    visible = _CONTROL_SHAPE.sub("", value)
    return sum(
        0
        if unicodedata.category(character).startswith("C")
        else 2
        if unicodedata.east_asian_width(character) in {"W", "F"}
        else 1
        for character in visible
    )


def _layout_limit(manual_id: str) -> int:
    lowered = manual_id.casefold()
    if "choice" in lowered:
        return 28
    if "dialogue" in lowered or "scrolling" in lowered:
        return 42
    if manual_id.startswith("plugins.js:"):
        return 32
    return 48


def _finding(
    kind: str,
    *,
    manual_id: str | None = None,
    candidate_id: str | None = None,
    status: str = "heuristic_review",
    details: Mapping[str, JsonValue] | None = None,
) -> dict[str, JsonValue]:
    value: dict[str, JsonValue] = {"kind": kind, "analysis_status": status}
    if manual_id is not None:
        value["manual_id"] = manual_id
    if candidate_id is not None:
        value["candidate_id"] = candidate_id
    if details is not None:
        value.update(details)
    return value


def _translation_findings(
    row: Mapping[str, JsonValue], terms: Sequence[tuple[str, str]]
) -> list[dict[str, JsonValue]]:
    manual_id = cast(str, row["manual_id"])
    state = cast(str, row["state"])
    if state == "pending":
        return [_finding("extracted_not_translated", manual_id=manual_id, status="confirmed_fact")]
    if state == "rejected":
        return [_finding("rejected_translation", manual_id=manual_id, status="confirmed_fact")]
    source_lines = cast(list[str], row["source"])
    translation_lines = cast(list[str], row["translation"])
    findings: list[dict[str, JsonValue]] = []
    if row["type"] == "fixed" and len(source_lines) != len(translation_lines):
        findings.append(
            _finding(
                "current_fixed_shape_mismatch",
                manual_id=manual_id,
                status="confirmed_fact",
                details={"source_slots": len(source_lines), "translation_slots": len(translation_lines)},
            )
        )
    for slot, (source, translation) in enumerate(zip(source_lines, translation_lines, strict=False), start=1):
        if not is_structural_blank(source) and is_structural_blank(translation):
            findings.append(
                _finding(
                    "blank_current_translation",
                    manual_id=manual_id,
                    status="confirmed_fact",
                    details={"slot": slot},
                )
            )
            continue
        source_words = set(_SOURCE_WORD.findall(_CONTROL_SHAPE.sub("", source)))
        residual = sorted(word for word in source_words if word in _CONTROL_SHAPE.sub("", translation))
        if residual:
            findings.append(
                _finding(
                    "source_residual",
                    manual_id=manual_id,
                    details={"slot": slot, "words": residual[:10], "translation_preview": translation[:160]},
                )
            )
        source_controls = Counter(_CONTROL_SHAPE.findall(source))
        translation_controls = Counter(_CONTROL_SHAPE.findall(translation))
        if source_controls != translation_controls:
            findings.append(
                _finding(
                    "control_shape_review",
                    manual_id=manual_id,
                    details={
                        "slot": slot,
                        "source": dict(source_controls),
                        "translation": dict(translation_controls),
                    },
                )
            )
        width = _display_width(translation)
        limit = _layout_limit(manual_id)
        if "\n" not in translation and width > limit:
            findings.append(
                _finding(
                    "layout_risk",
                    manual_id=manual_id,
                    details={"slot": slot, "display_width": width, "heuristic_limit": limit},
                )
            )
    source_text = "\n".join(source_lines)
    translation_text = "\n".join(translation_lines)
    for trigger, expected in terms:
        if trigger in source_text and expected not in translation_text:
            findings.append(
                _finding(
                    "terminology_mismatch",
                    manual_id=manual_id,
                    details={"trigger": trigger, "expected_translation": expected},
                )
            )
    return findings


def _survey_findings(
    locations: Sequence[Mapping[str, JsonValue]], exported_ids: set[str], *, check_builtin: bool
) -> list[dict[str, JsonValue]]:
    findings: list[dict[str, JsonValue]] = []
    for location in locations:
        classification = location.get("classification")
        expected = location.get("expected_manual_id")
        candidate = location.get("candidate_id")
        if (
            check_builtin
            and classification == "builtin"
            and isinstance(expected, str)
            and expected not in exported_ids
        ):
            findings.append(_finding("extracted_unit_missing", manual_id=expected, status="confirmed_fact"))
            continue
        roles = location.get("roles")
        if (
            check_builtin
            and classification == "review"
            and isinstance(roles, list)
            and any(role in {"display", "display_candidate"} for role in roles)
        ):
            findings.append(
                _finding(
                    "possible_unextracted_player_text",
                    candidate_id=candidate if isinstance(candidate, str) else None,
                    details={"source": location.get("source", "")},
                )
            )
    return findings


def _generic_findings(
    rows: Sequence[Mapping[str, JsonValue]], manifest: Mapping[str, tuple[str, str]]
) -> list[dict[str, JsonValue]]:
    exported = {cast(str, row["manual_id"]): row for row in rows}
    findings: list[dict[str, JsonValue]] = []
    for manual_id, (candidate_id, original) in manifest.items():
        row = exported.get(manual_id)
        if row is None:
            findings.append(
                _finding(
                    "generic_unit_missing",
                    manual_id=manual_id,
                    candidate_id=candidate_id,
                    status="confirmed_fact",
                )
            )
            continue
        if row.get("source") != [original] or row.get("type") != "free":
            findings.append(
                _finding(
                    "generic_source_mapping_mismatch",
                    manual_id=manual_id,
                    candidate_id=candidate_id,
                    status="confirmed_fact",
                )
            )
    for manual_id in sorted(set(exported) - set(manifest)):
        findings.append(_finding("generic_unit_unexpected", manual_id=manual_id, status="confirmed_fact"))
    return findings


def _write_back_findings(path: Path | None) -> tuple[list[dict[str, JsonValue]], list[str]]:
    if path is None:
        return [], ["write_back_preview_missing"]
    report = read_json_object(path, "WriteBack 预览或验证报告")
    problem = (
        report.get("source_unchanged") is not True
        or report.get("output_json_valid") is not True
        or report.get("structural_differences") not in {0, None}
        or bool(report.get("non_text_value_changes"))
        or any(
            bool(report.get(field))
            for field in (
                "changed_source_files",
                "missing_source_files",
                "added_source_key_files",
                "invalid_output_json",
                "missing_output_key_files",
                "added_output_data_files",
                "changed_output_core_files",
            )
        )
    )
    return (
        [_finding("write_back_preview_issue", status="confirmed_fact")] if problem else [],
        [],
    )


def _runtime_findings(path: Path | None) -> tuple[list[dict[str, JsonValue]], list[str]]:
    if path is None:
        return [], ["runtime_observation_missing"]
    report = read_json_object(path, "NW.js 运行时报告")
    status = report.get("qa_status")
    if status not in {"clean", "needs_review", "unverified"}:
        fail(str(path), "运行时报告缺少有效 qa_status", "使用 inspect_nwjs_runtime.py 当前报告")
    if status == "needs_review":
        return [_finding("runtime_observation_issue", status="confirmed_fact")], []
    if status == "unverified":
        return [], ["runtime_observation_unverified"]
    return [], []


def _scan(args: argparse.Namespace) -> int:
    started = time.perf_counter()
    translations_path = require_file(args.translations, "ATT Translation export JSONL")
    survey_root = require_directory(args.survey, "survey 作业目录")
    generic_manifest_path = cast(Path | None, args.generic_manifest)
    terminology_path = cast(Path | None, args.terminology)
    write_back_path = cast(Path | None, args.write_back_preview)
    runtime_path = cast(Path | None, args.runtime_report)
    inputs = [translations_path, survey_root]
    inputs.extend(
        path
        for path in (generic_manifest_path, terminology_path, write_back_path, runtime_path)
        if path is not None
    )
    protect_outputs([args.output], inputs=inputs, replace=args.replace)
    survey, locations, _groups, baseline = load_survey(survey_root)
    verify_source_baseline(survey, baseline)
    rows = _read_translation_export(translations_path)
    generic_manifest = _generic_manifest(generic_manifest_path)
    owner_values: set[str | None] = set()
    for row in rows:
        owner = row.get("owner")
        owner_values.add(owner if isinstance(owner, str) else None)
    if None in owner_values and len(owner_values) > 1:
        fail(str(translations_path), "RPG Maker 与 Generic 导出不能混在一个文件", "分别执行译后 QA")
    generic_project = generic_manifest is not None or owner_values == {None}
    if generic_project and any(row.get("owner") is not None for row in rows):
        fail(
            str(translations_path),
            "Generic manifest 与 RPG Maker Translation export 不匹配",
            "使用对应项目的导出",
        )
    terms = _terminology(terminology_path)
    findings = [finding for row in rows for finding in _translation_findings(row, terms)]
    findings.extend(
        _survey_findings(
            locations,
            {cast(str, row["manual_id"]) for row in rows},
            check_builtin=not generic_project,
        )
    )
    if generic_project:
        if generic_manifest is None:
            unverified = ["generic_manifest_missing"]
        else:
            findings.extend(_generic_findings(rows, generic_manifest))
            unverified = []
    else:
        unverified = []
    write_back_findings, write_back_unverified = _write_back_findings(write_back_path)
    runtime_findings, runtime_unverified = _runtime_findings(runtime_path)
    findings.extend(write_back_findings)
    findings.extend(runtime_findings)
    unverified.extend(write_back_unverified)
    unverified.extend(runtime_unverified)
    for number, finding in enumerate(findings, start=1):
        finding["finding_id"] = f"finding-{number:06d}"
    actionable_ids = {
        cast(str, finding["manual_id"]) for finding in findings if isinstance(finding.get("manual_id"), str)
    }
    revision_ids = [cast(str, row["manual_id"]) for row in rows if row["manual_id"] in actionable_ids]
    status = "needs_review" if findings else "unverified" if unverified else "clean"
    counts = Counter(cast(str, finding["kind"]) for finding in findings)
    summary: dict[str, JsonValue] = {
        "qa_status": status,
        "translation_export": _file_fact(translations_path, "ATT Translation export JSONL"),
        "translations": len(rows),
        "findings": len(findings),
        "counts": dict(sorted(counts.items())),
        "revision_ids": revision_ids,
        "unverified": unverified,
        "survey_game_root": survey.get("game_root", ""),
    }
    metrics: dict[str, JsonValue] = {
        "translation_entries_scanned": len(rows),
        "survey_locations_checked": len(locations),
        "revision_ids": len(revision_ids),
        "local_command_elapsed_ms": round((time.perf_counter() - started) * 1000),
        "external_request_wait_ms": 0,
    }
    atomic_write_directory(
        args.output,
        {
            "qa-summary.json": _json_text(summary),
            "findings.jsonl": _json_lines(findings),
            "agent-work-metrics.json": _json_text(metrics),
        },
        replace=args.replace,
    )
    print(f"译后 QA：{status}；发现 {len(findings)} 项，建议集中修订 {len(revision_ids)} 个自然 Unit。")
    print(f"QA 目录：{display_path(args.output)}")
    return 0


def _manual(args: argparse.Namespace) -> int:
    scan_root = require_directory(args.scan, "QA scan 作业目录")
    summary = read_json_object(scan_root / "qa-summary.json", "qa-summary.json", allowed_root=scan_root)
    export_fact = summary.get("translation_export")
    revision_ids = summary.get("revision_ids")
    if not isinstance(export_fact, dict) or not isinstance(revision_ids, list):
        fail(str(scan_root), "QA 摘要缺少输入基线或自然 ID", "重新运行 translation_qa.py scan")
    path_value = export_fact.get("path")
    bytes_value = export_fact.get("bytes")
    digest_value = export_fact.get("sha256")
    if (
        not isinstance(path_value, str)
        or not isinstance(bytes_value, int)
        or not isinstance(digest_value, str)
        or any(not isinstance(value, str) or not value for value in revision_ids)
    ):
        fail(str(scan_root), "QA 摘要输入基线或自然 ID 无效", "重新运行 translation_qa.py scan")
    current = _file_fact(Path(path_value), "QA 使用的 Translation export")
    if current["bytes"] != bytes_value or current["sha256"] != digest_value:
        fail(path_value, "Translation export 在 QA 后发生变化", "重新运行 translation_qa.py scan")
    protect_outputs([args.output], inputs=[scan_root, Path(path_value)], replace=args.replace)
    rows = [{"manual_id": cast(str, value)} for value in revision_ids]
    atomic_write_text(args.output, _json_lines(rows), replace=args.replace)
    print(f"已输出 {len(rows)} 个自然 ID：{display_path(args.output)}")
    print("下一步：把该文件交给 att mv|mz|generic manual export --ids，取得数据库当前译文的预填 Manual。")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _scan(parsed) if parsed.command == "scan" else _manual(parsed))
