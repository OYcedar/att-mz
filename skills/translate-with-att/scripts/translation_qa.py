#!/usr/bin/env python3
"""从 ATT Translation export 与 Survey 或 Generic 输入事实生成非阻断译后 QA。"""

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
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
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
    parse_json_text,
    protect_outputs,
    read_json_object,
    require_directory,
    require_file,
    require_file_within,
    run_cli,
    safe_walk_files,
    validate_object_keys,
)
from att_toolbox.png import decode_png_size
from att_toolbox.rpg_control_codes import is_structural_blank
from att_toolbox.survey import GENERIC_EVIDENCE_FIELDS, load_survey, verify_source_baseline
from att_toolbox.survey_projection import (
    project_builtin_units,
    project_rule_units,
    read_rules_manifest,
)
from att_toolbox.translation_export import read_translation_export

_CONTROL_SHAPE = re.compile(
    r"(?:\\|\x1b)(?:[A-Za-z]+(?:\[[^\]\r\n]*\]|<[^>\r\n]*>)?|[\\{}$.|!><^])|"
    r"\{\{[^{}\r\n]+\}\}|\$\{[^{}\r\n]+\}|%[A-Za-z_][A-Za-z0-9_]*%|%[0-9]+|"
    r"</?[A-Za-z][^>\r\n]*>|\f"
)
_REVIEW_EXAMPLE_LIMIT = 5
_RUNTIME_SMOKE_SCENARIOS = ("title", "new_game", "dialogue", "menu", "quest_log", "options", "save")


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="扫描 ATT 当前译文、来源事实及可选输出证据，生成非阻断 QA。")
    subparsers = parser.add_subparsers(dest="command", required=True)
    scan = subparsers.add_parser("scan", help="扫描当前 Translation export")
    scan.add_argument("--translations", type=Path, required=True, help="ATT translation export JSONL")
    evidence = scan.add_mutually_exclusive_group(required=True)
    evidence.add_argument("--survey", type=Path, help="同一来源快照的 survey 作业目录")
    evidence.add_argument("--generic-input", type=Path, help="独立 Generic 项目的当前 JSONL 输入根")
    scan.add_argument("--coverage", type=Path, help="同一次 finalize 生成的 coverage.json")
    scan.add_argument(
        "--generic-manifest",
        type=Path,
        help="Survey finalize 生成的可选 Generic 精确映射",
    )
    scan.add_argument("--terminology", type=Path, help="本次 Translate 使用的 terminology.toml")
    scan.add_argument("--write-back", type=Path, help="可选 ATT 当前项目的实际 write_back 目录")
    scan.add_argument(
        "--runtime-report",
        type=Path,
        help="Survey 模式可选的 NW.js smoke/observe report.json",
    )
    scan.add_argument("--output", type=Path, required=True, help="QA 作业目录")
    scan.add_argument("--replace", action="store_true")
    manual = subparsers.add_parser("manual", help="只输出供 att manual export --ids 使用的自然 ID JSONL")
    manual.add_argument("--scan", type=Path, required=True, help="scan 生成的 QA 作业目录")
    manual.add_argument(
        "--review-group",
        action="append",
        default=[],
        help="加入一个已审核的 review-groups.jsonl 组；可重复",
    )
    manual.add_argument("--output", type=Path, required=True, help="自然 ID JSONL")
    manual.add_argument("--replace", action="store_true")
    return parser


def _json_text(value: JsonValue) -> str:
    return json.dumps(value, ensure_ascii=False, indent=2) + "\n"


def _json_lines(values: Sequence[Mapping[str, JsonValue]]) -> str:
    return "".join(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n" for value in values)


def _read_jsonl_objects(path: Path, description: str) -> list[dict[str, JsonValue]]:
    source = require_file(path, description)
    rows: list[dict[str, JsonValue]] = []
    for line_number, line in enumerate(source.read_text(encoding="utf-8-sig").splitlines(), start=1):
        if not line.strip():
            fail(str(source), f"第 {line_number} 行为空", f"重新运行 {description}")
        raw = parse_json_text(line, f"{source} 第 {line_number} 行")
        if not isinstance(raw, dict):
            fail(str(source), f"第 {line_number} 行不是 object", f"重新运行 {description}")
        rows.append(dict(raw))
    return rows


def _group_heuristic_findings(
    findings: Sequence[dict[str, JsonValue]],
) -> list[dict[str, JsonValue]]:
    grouped: dict[str, list[dict[str, JsonValue]]] = {}
    for finding in findings:
        if finding.get("analysis_status") != "heuristic_review":
            continue
        kind = finding.get("kind")
        if not isinstance(kind, str):
            fail("QA findings", "启发式 finding 缺少 kind", "重新运行 translation_qa.py scan")
        grouped.setdefault(kind, []).append(finding)

    output: list[dict[str, JsonValue]] = []
    for number, (kind, members) in enumerate(grouped.items(), start=1):
        group_id = f"review-{number:06d}"
        for member in members:
            member["review_group_id"] = group_id
        manual_ids = {
            cast(str, member["manual_id"]) for member in members if isinstance(member.get("manual_id"), str)
        }
        candidate_ids = {
            cast(str, member["candidate_id"])
            for member in members
            if isinstance(member.get("candidate_id"), str)
        }
        output.append(
            {
                "review_group_id": group_id,
                "kind": kind,
                "findings": len(members),
                "manual_ids": len(manual_ids),
                "candidate_ids": len(candidate_ids),
                "examples": [dict(member) for member in members[:_REVIEW_EXAMPLE_LIMIT]],
            }
        )
    return output


def _file_fact(path: Path, description: str) -> dict[str, JsonValue]:
    source = require_file(path, description)
    raw = source.read_bytes()
    return {"path": str(source.resolve()), "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}


def _read_translation_export(path: Path) -> list[dict[str, JsonValue]]:
    return read_translation_export(path)


@dataclass(frozen=True)
class _GenericTreeUnit:
    manual_id: str
    relative_path: str
    group_id: str
    kind: str
    unit_id: str
    text: str


@dataclass(frozen=True)
class _GenericTree:
    root: Path
    files: tuple[str, ...]
    units: tuple[_GenericTreeUnit, ...]
    fact: dict[str, JsonValue]


def _generic_jsonl_lines(path: Path, description: str) -> tuple[list[str], bytes]:
    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        fail(str(path), f"{description}不是有效 UTF-8：{error}", "修正 JSONL 后重新运行 QA")
    normalized = text.replace("\r\n", "\n")
    if "\r" in normalized:
        fail(str(path), f"{description}包含非 CRLF 的 CR", "只使用 LF 或 CRLF 物理行")
    if not normalized:
        return [], raw
    lines = normalized.split("\n")
    if lines[-1] == "":
        lines.pop()
    return lines, raw


def _read_generic_tree(root_path: Path, description: str, *, only_jsonl: bool) -> _GenericTree:
    root = require_directory(root_path, description)
    all_files = sorted(
        safe_walk_files(root),
        key=lambda path: path.relative_to(root).as_posix().encode("utf-8"),
    )
    for path in all_files:
        try:
            link_count = path.stat().st_nlink
        except OSError as error:
            fail(str(path), f"无法读取文件身份：{error}", "修正文件系统错误后重新运行 QA")
        if link_count != 1:
            fail(str(path), f"文件有 {link_count} 个硬链接", "使用没有硬链接的当前 Generic 输入或输出")
    non_jsonl = [path for path in all_files if path.suffix != ".jsonl"]
    if only_jsonl and non_jsonl:
        fail(
            str(root),
            f"{description}包含非 JSONL 文件 {non_jsonl[0].relative_to(root).as_posix()}",
            "传入 ATT 当前 Generic 项目的实际 write_back 目录",
        )
    jsonl_files = [path for path in all_files if path.suffix == ".jsonl"]
    digest = hashlib.sha256()
    relative_files: list[str] = []
    units: list[_GenericTreeUnit] = []
    group_ids: set[str] = set()
    group_count = 0
    for path in jsonl_files:
        relative = path.relative_to(root).as_posix()
        relative_files.append(relative)
        lines, raw = _generic_jsonl_lines(path, description)
        if only_jsonl and (b"\r" in raw or (raw and not raw.endswith(b"\n"))):
            fail(
                str(path),
                "Generic write_back 没有使用 LF，或非空文件末尾缺少 LF",
                "重新执行 Generic WriteBack",
            )
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(hashlib.sha256(raw).digest())
        for line_number, line in enumerate(lines, start=1):
            if not line.strip():
                fail(str(path), f"第 {line_number} 行为空", "删除空白物理行后重新运行 QA")
            raw_group = parse_json_text(line, f"{path} 第 {line_number} 行")
            if not isinstance(raw_group, dict):
                fail(str(path), f"第 {line_number} 行不是 Group object", "修正 Generic JSONL")
            group = dict(raw_group)
            validate_object_keys(group, f"{path} 第 {line_number} 行", {"id", "kind", "units"})
            if set(group) != {"id", "kind", "units"}:
                fail(str(path), f"第 {line_number} 行缺少 Group 字段", "补齐 id、kind 和 units")
            group_id = group.get("id")
            kind = group.get("kind")
            raw_units = group.get("units")
            if (
                not isinstance(group_id, str)
                or group_id == ""
                or group_id in group_ids
                or not isinstance(kind, str)
                or kind == ""
                or not isinstance(raw_units, list)
                or not raw_units
            ):
                fail(
                    str(path),
                    f"第 {line_number} 行 Group 身份重复、为空或 units 无效",
                    "按 Generic JSONL 规格修正输入",
                )
            group_ids.add(group_id)
            group_count += 1
            unit_ids: set[str] = set()
            for unit_number, raw_unit in enumerate(raw_units, start=1):
                if not isinstance(raw_unit, dict):
                    fail(
                        str(path),
                        f"第 {line_number} 行第 {unit_number} 项不是 Unit object",
                        "修正 Generic JSONL",
                    )
                unit = dict(raw_unit)
                validate_object_keys(
                    unit,
                    f"{path} 第 {line_number} 行第 {unit_number} 项",
                    {"id", "text"},
                )
                if set(unit) != {"id", "text"}:
                    fail(
                        str(path),
                        f"第 {line_number} 行第 {unit_number} 项缺少 Unit 字段",
                        "补齐 id 和 text",
                    )
                unit_id = unit.get("id")
                text = unit.get("text")
                if (
                    not isinstance(unit_id, str)
                    or unit_id == ""
                    or unit_id in unit_ids
                    or not isinstance(text, str)
                    or "\r" in text
                    or "\0" in text
                ):
                    fail(
                        str(path),
                        f"第 {line_number} 行第 {unit_number} 项身份重复、为空或 text 无效",
                        "按 Generic JSONL 规格修正输入",
                    )
                unit_ids.add(unit_id)
                units.append(
                    _GenericTreeUnit(
                        manual_id=f"{relative}:line{line_number}:unit{unit_number}:text",
                        relative_path=relative,
                        group_id=group_id,
                        kind=kind,
                        unit_id=unit_id,
                        text=text,
                    )
                )
    return _GenericTree(
        root=root,
        files=tuple(relative_files),
        units=tuple(units),
        fact={
            "path": str(root),
            "files": len(relative_files),
            "groups": group_count,
            "units": len(units),
            "sha256": digest.hexdigest(),
        },
    )


def _standalone_generic_recipes(tree: _GenericTree) -> dict[str, dict[str, JsonValue]]:
    return {
        unit.manual_id: {
            "input_file": f"generic/input/{unit.relative_path}",
            "group_id": unit.group_id,
            "kind": unit.kind,
            "unit_id": unit.unit_id,
            "source": unit.text,
        }
        for unit in tree.units
    }


def _bind_generic_export_to_input(rows: Sequence[Mapping[str, JsonValue]], tree: _GenericTree) -> None:
    actual_ids = [cast(str, row["manual_id"]) for row in rows]
    expected_ids = [unit.manual_id for unit in tree.units]
    if actual_ids != expected_ids:
        fail(
            "Translation export",
            "Generic 导出的自然 ID 集合或顺序与当前 JSONL 输入不一致",
            "对该输入重新执行 Extract，再导出完整 translation export",
        )
    for row, unit in zip(rows, tree.units, strict=True):
        if (
            row.get("source") != unit.text.split("\n")
            or row.get("type") != "free"
            or row.get("owner") is not None
            or row.get("rule_number") is not None
        ):
            fail(
                "Translation export",
                f"{unit.manual_id} 与当前 JSONL 输入的原文或 Generic 类型不一致",
                "对该输入重新执行 Extract，再导出完整 translation export",
            )


def _generic_manifest(path: Path | None) -> dict[str, dict[str, JsonValue]] | None:
    if path is None:
        return None
    source = require_file(path, "Generic 精确映射 manifest")
    root = read_json_object(source, "Generic 精确映射 manifest")
    validate_object_keys(root, str(source), {"sources", "decisions", "recipes"})
    recipes = root.get("recipes")
    if not isinstance(recipes, list):
        fail(str(source), "recipes 不是 array", "重新运行 rpg_maker_survey.py finalize")
    output: dict[str, dict[str, JsonValue]] = {}
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
        typed = dict(raw)
        output[manual_id] = typed
        seen_candidates.add(candidate_id)
    return output


def _generic_recipe_relative_path(recipe: Mapping[str, JsonValue], description: str) -> str:
    input_file = recipe.get("input_file")
    if not isinstance(input_file, str):
        fail(description, "Generic recipe 缺少 input_file", "重新运行 rpg_maker_survey.py finalize")
    parts = PurePosixPath(input_file).parts
    if (
        len(parts) < 3
        or parts[:2] != ("generic", "input")
        or ".." in parts
        or PurePosixPath(input_file).as_posix() != input_file
    ):
        fail(description, "Generic recipe input_file 不在 generic/input 下", "重新运行 finalize")
    relative = PurePosixPath(*parts[2:]).as_posix()
    if not relative.endswith(".jsonl"):
        fail(description, "Generic recipe input_file 不是 JSONL", "重新运行 finalize")
    return relative


def _coverage_projection(
    path: Path,
    survey: Mapping[str, JsonValue],
    locations: Sequence[Mapping[str, JsonValue]],
    groups: Sequence[Mapping[str, JsonValue]],
    rules_manifest: Sequence[Mapping[str, JsonValue]],
) -> tuple[dict[str, dict[str, JsonValue]], set[str], bool]:
    coverage = read_json_object(path, "finalize coverage.json")
    engine = survey.get("engine")
    if engine not in {"mv", "mz"} or coverage.get("engine") != engine:
        fail(str(path), "coverage 与 survey 引擎不一致", "使用同一次 survey finalize 生成的 coverage.json")
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
            or owner not in {"builtin", "rules"}
            or not isinstance(item.get("source_text"), str)
            or not isinstance(item.get("source"), str)
        ):
            fail(str(path), f"unit_projection 第 {number} 项身份或字段无效", "重新运行 finalize")
        if owner == "rules":
            if not isinstance(rule_number, int) or isinstance(rule_number, bool) or rule_number <= 0:
                fail(str(path), f"{manual_id} 缺少自然 rule_number", "重新运行 finalize")
        elif rule_number is not None:
            fail(str(path), f"{manual_id} 不应包含 rule_number", "重新运行 finalize")
        location = locations_by_id[candidate_id]
        if location.get("source") != item["source"] or (
            owner == "builtin" and location.get("source_text") != item["source_text"]
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
        if not isinstance(manual_id, str) or manual_id in ownership or owner not in {"builtin", "rules"}:
            fail(str(path), f"expected_ownership 第 {number} 项无效", "重新运行 finalize")
        normalized_rule = (
            rule_number
            if isinstance(rule_number, int) and not isinstance(rule_number, bool) and rule_number > 0
            else None
        )
        if (owner == "rules") != (normalized_rule is not None):
            fail(str(path), f"{manual_id} 的预期所有权字段矛盾", "重新运行 finalize")
        ownership[manual_id] = (cast(str, owner), normalized_rule)
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
            validate_object_keys(
                evidence,
                f"{path} dispositions 第 {number} 项 evidence",
                set(GENERIC_EVIDENCE_FIELDS),
            )
            if any(
                not isinstance(evidence.get(field), str) or not cast(str, evidence[field]).strip()
                for field in GENERIC_EVIDENCE_FIELDS
            ):
                fail(str(path), f"dispositions 第 {number} 项 Generic 证据无效", "重新运行 finalize")
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
        candidates = raw.get("candidate_ids")
        if isinstance(candidates, list):
            if any(not isinstance(value, str) or value not in locations_by_id for value in candidates):
                fail(str(path), f"unresolved 第 {number} 项候选无效", "重新运行 finalize")
            resolved_review_candidates.extend(cast(list[str], candidates))
            continue
        target = raw.get("target")
        if not isinstance(target, str):
            fail(str(path), f"unresolved 第 {number} 项缺少自然 target", "重新运行 finalize")
        if target.startswith("candidate:"):
            candidate_id = target.removeprefix("candidate:")
            if candidate_id not in locations_by_id:
                fail(str(path), f"unresolved 第 {number} 项引用未知候选", "重新运行 finalize")
            resolved_review_candidates.append(candidate_id)
        elif target.startswith("group:"):
            group_id = target.removeprefix("group:")
            if group_id not in group_members:
                fail(str(path), f"unresolved 第 {number} 项引用未知关系组", "重新运行 finalize")
            resolved_review_candidates.extend(group_members[group_id])
        else:
            fail(str(path), f"unresolved 第 {number} 项 target 无效", "重新运行 finalize")
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
    return projection, generic_candidates, complete_value


def _bind_rpg_export_to_coverage(
    rows: Sequence[Mapping[str, JsonValue]], projection: Mapping[str, Mapping[str, JsonValue]]
) -> None:
    exported = {cast(str, row["manual_id"]): row for row in rows}
    if set(exported) != set(projection):
        fail(
            "Translation export",
            "导出的自然 ID 集合与 coverage Unit 投影不一致",
            "使用该 survey/coverage 对应 ATT 项目的完整 translation export",
        )
    for manual_id, row in exported.items():
        expected = projection[manual_id]
        expected_rule = expected.get("rule_number")
        actual_rule = row.get("rule_number")
        if (
            "\n".join(cast(list[str], row["source"])) != expected.get("source_text")
            or row.get("owner") != expected.get("owner")
            or actual_rule != expected_rule
        ):
            fail(
                "Translation export",
                f"{manual_id} 与 coverage 的来源或所有权不一致",
                "使用同一项目最新 Extract 后的完整 translation export",
            )


def _bind_generic_manifest(
    manifest: Mapping[str, Mapping[str, JsonValue]],
    generic_candidates: set[str],
    locations: Sequence[Mapping[str, JsonValue]],
) -> None:
    locations_by_id = {
        cast(str, item["candidate_id"]): item
        for item in locations
        if isinstance(item.get("candidate_id"), str)
    }
    manifest_candidates = {
        cast(str, recipe["candidate_id"])
        for recipe in manifest.values()
        if isinstance(recipe.get("candidate_id"), str)
    }
    if manifest_candidates != generic_candidates:
        fail(
            "Generic manifest",
            "manifest 候选集合与 coverage 的 Generic 归属不一致",
            "使用同一次 finalize 生成的 coverage.json 和 generic/manifest.json",
        )
    for manual_id, recipe in manifest.items():
        candidate_id = cast(str, recipe["candidate_id"])
        location = locations_by_id[candidate_id]
        if recipe.get("source") != location.get("source_text") or recipe.get("physical_file") != location.get(
            "physical_file"
        ):
            fail(
                "Generic manifest",
                f"{manual_id} 与 survey 候选内容或物理来源不一致",
                "使用同一次 survey finalize 生成的 Generic manifest",
            )


def _bind_generic_export_to_manifest(
    rows: Sequence[Mapping[str, JsonValue]],
    manifest: Mapping[str, Mapping[str, JsonValue]],
) -> None:
    exported = {cast(str, row["manual_id"]): row for row in rows}
    if set(exported) != set(manifest):
        fail(
            "Translation export",
            "Generic 导出的自然 ID 集合与 manifest 不一致",
            "使用该 manifest 输入建立的 ATT Generic 项目完整导出",
        )
    for manual_id, row in exported.items():
        if (
            row.get("source") != [manifest[manual_id].get("source")]
            or row.get("type") != "free"
            or row.get("owner") is not None
        ):
            fail(
                "Translation export",
                f"{manual_id} 与 Generic manifest 的原文或类型不一致",
                "使用对应项目最新 Extract 后的完整 translation export",
            )


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


def _source_script(character: str) -> str | None:
    """只按原文中可复核的文字序列寻找残留，不把任意非 ASCII 当成源语。"""

    if character in {"'", "-", "’"}:
        return "connector"
    if not unicodedata.category(character).startswith(("L", "M")):
        return None
    name = unicodedata.name(character, "")
    if "HIRAGANA" in name or "KATAKANA" in name:
        return "kana"
    if "HANGUL" in name:
        return "hangul"
    if "CJK" in name or "IDEOGRAPH" in name:
        return "cjk"
    if "LATIN" in name:
        return "latin"
    if "CYRILLIC" in name:
        return "cyrillic"
    if "GREEK" in name:
        return "greek"
    return "other_letter"


def _text_tokens(value: str) -> set[str]:
    visible = _CONTROL_SHAPE.sub("", value)
    tokens: set[str] = set()
    current: list[str] = []
    current_script: str | None = None

    def finish() -> None:
        nonlocal current, current_script
        if current_script is not None:
            token = "".join(current).strip("-'\u2019")
            minimum = 3 if current_script == "cjk" else 2
            if len(token) >= minimum:
                tokens.add(token)
        current = []
        current_script = None

    for character in visible:
        script = _source_script(character)
        if script == "connector" and current_script in {"latin", "cyrillic", "greek", "other_letter"}:
            current.append(character)
            continue
        if script is None:
            finish()
            continue
        if current_script == script:
            current.append(character)
            continue
        finish()
        current_script = script
        current.append(character)
    finish()
    return tokens


def _source_residual_tokens(source: str, translation: str) -> list[str]:
    return sorted(_text_tokens(source).intersection(_text_tokens(translation)))


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
        residual = _source_residual_tokens(source, translation)
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


def _directory_fact(root: Path) -> dict[str, JsonValue]:
    digest = hashlib.sha256()
    files = sorted(
        safe_walk_files(root),
        key=lambda path: path.relative_to(root).as_posix().encode("utf-8"),
    )
    for path in files:
        relative = path.relative_to(root).as_posix()
        raw = path.read_bytes()
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(hashlib.sha256(raw).digest())
    return {"path": str(root), "files": len(files), "sha256": digest.hexdigest()}


def _semantic_characters(value: str) -> str:
    return "".join(
        character.casefold()
        for character in _CONTROL_SHAPE.sub("", value)
        if unicodedata.category(character).startswith(("L", "N"))
    )


def _write_back_stable_characters(value: str) -> str:
    return "".join(
        character
        for character in _CONTROL_SHAPE.sub("", value)
        if unicodedata.category(character).startswith(("L", "M", "N", "S"))
    )


def _write_back_residual(source: str, expected_text: str, output_text: str) -> tuple[list[str], list[str]]:
    source_tokens = _text_tokens(source)
    actual = source_tokens.intersection(_text_tokens(output_text))
    expected = source_tokens.intersection(_text_tokens(expected_text))
    return sorted(actual), sorted(actual - expected)


def _generic_write_back_findings(
    root: Path,
    rows: Sequence[Mapping[str, JsonValue]],
    manifest: Mapping[str, Mapping[str, JsonValue]],
    *,
    expected_files: set[str] | None,
    strict_export_text: bool,
) -> tuple[list[dict[str, JsonValue]], bool, dict[str, JsonValue]]:
    tree = _read_generic_tree(root, "Generic write_back 目录", only_jsonl=True)
    required_files = (
        expected_files
        if expected_files is not None
        else {_generic_recipe_relative_path(recipe, "Generic manifest") for recipe in manifest.values()}
    )
    if set(tree.files) != required_files:
        fail(
            str(root),
            "Generic write_back 文件集合与当前输入不一致",
            "重新执行该 Generic 项目的 WriteBack，并传入实际输出目录",
        )
    actual_units = {unit.manual_id: unit for unit in tree.units}
    if set(actual_units) != set(manifest):
        fail(
            str(root),
            "Generic write_back Unit 集合与当前输入不一致",
            "重新执行该 Generic 项目的 WriteBack",
        )
    exported = {cast(str, row["manual_id"]): row for row in rows}
    findings: list[dict[str, JsonValue]] = []
    transformed_current = False
    for manual_id, recipe in manifest.items():
        actual = actual_units[manual_id]
        output_text = actual.text
        if (
            actual.relative_path != _generic_recipe_relative_path(recipe, "Generic recipe")
            or actual.group_id != recipe.get("group_id")
            or actual.kind != recipe.get("kind")
            or actual.unit_id != recipe.get("unit_id")
        ):
            fail(
                str(root),
                f"{manual_id} 的 Group、Unit 身份或 kind 与当前输入不一致",
                "重新执行 WriteBack",
            )
        row = exported[manual_id]
        source = "\n".join(cast(list[str], row["source"]))
        if row.get("state") == "current":
            expected_text = "\n".join(cast(list[str], row["translation"]))
            if strict_export_text and output_text != expected_text:
                same_semantic_text = _write_back_stable_characters(
                    output_text
                ) == _write_back_stable_characters(expected_text)
                same_controls = _CONTROL_SHAPE.findall(output_text) == _CONTROL_SHAPE.findall(expected_text)
                if not same_semantic_text or not same_controls:
                    fail(
                        str(root),
                        f"{manual_id} 的 WriteBack 正文不对应 Current 译文",
                        "用当前 translation export 重新执行 Generic WriteBack",
                    )
                transformed_current = True
        else:
            expected_text = source
            if output_text != source:
                if strict_export_text:
                    fail(
                        str(root),
                        f"{manual_id} 的未接受正文没有保留当前原文",
                        "用当前 translation export 重新执行 Generic WriteBack",
                    )
                findings.append(
                    _finding(
                        "write_back_unaccepted_text_changed",
                        manual_id=manual_id,
                        status="confirmed_fact",
                        details={"candidate_id": recipe.get("candidate_id")},
                    )
                )
        retained_source = (
            row.get("state") == "current"
            and output_text == source
            and _semantic_characters(source) != _semantic_characters(expected_text)
        )
        if retained_source:
            findings.append(
                _finding(
                    "write_back_retained_source",
                    manual_id=manual_id,
                    status="confirmed_fact",
                    details={"candidate_id": recipe.get("candidate_id")},
                )
            )
            continue
        residual, introduced = _write_back_residual(source, expected_text, output_text)
        if residual:
            findings.append(
                _finding(
                    "write_back_source_residual",
                    manual_id=manual_id,
                    details={
                        "candidate_id": recipe.get("candidate_id"),
                        "words": residual[:10],
                        "introduced_words": introduced[:10],
                    },
                )
            )
    if _read_generic_tree(root, "Generic write_back 目录", only_jsonl=True) != tree:
        fail(
            str(root),
            "Generic write_back 在 QA 期间发生变化",
            "等待 WriteBack 输出稳定后重新执行完整 QA",
        )
    return (
        findings,
        transformed_current,
        {
            "path": tree.fact["path"],
            "files": tree.fact["files"],
            "sha256": tree.fact["sha256"],
        },
    )


def _rpg_output_relative(manual_id: str, engine: str) -> str | None:
    source_name = manual_id.split(":", 1)[0]
    prefix = "www/" if engine == "mv" else ""
    if source_name == "plugins.js":
        return f"{prefix}js/plugins.js"
    if source_name.endswith(".json") and "/" not in source_name and "\\" not in source_name:
        return f"{prefix}data/{source_name}"
    return None


def _rpg_write_back_findings(
    root: Path,
    rows: Sequence[Mapping[str, JsonValue]],
    engine: str,
) -> tuple[list[dict[str, JsonValue]], list[str]]:
    actual_files = {path.relative_to(root).as_posix(): path for path in safe_walk_files(root)}
    data_prefix = "www/data/" if engine == "mv" else "data/"
    output_tokens: dict[str, set[str]] = {}
    for relative, path in actual_files.items():
        if relative.startswith(data_prefix) and relative.endswith(".json"):
            try:
                text = path.read_text(encoding="utf-8-sig")
            except UnicodeError:
                fail(str(path), "RPG Maker write_back JSON 不是有效 UTF-8", "重新执行 WriteBack")
            parsed = parse_json_text(text, str(path))
            pending: list[JsonValue] = [parsed]
            tokens: set[str] = set()
            while pending:
                value = pending.pop()
                if isinstance(value, str):
                    tokens.update(_text_tokens(value))
                elif isinstance(value, list):
                    pending.extend(value)
                elif isinstance(value, dict):
                    pending.extend(value.values())
            output_tokens[relative] = tokens
        elif relative == ("www/js/plugins.js" if engine == "mv" else "js/plugins.js"):
            try:
                output_tokens[relative] = _text_tokens(path.read_text(encoding="utf-8-sig"))
            except UnicodeError:
                fail(str(path), "RPG Maker write_back plugins.js 不是有效 UTF-8", "重新执行 WriteBack")
    required_by_id: dict[str, str] = {}
    source_tokens_by_file: dict[str, dict[str, set[str]]] = {}
    expected_tokens_by_id: dict[str, set[str]] = {}
    unverified: list[str] = []
    for row in rows:
        manual_id = cast(str, row["manual_id"])
        relative = _rpg_output_relative(manual_id, engine)
        if relative is None:
            unverified.append(f"write_back_location_unmapped:{manual_id}")
        else:
            required_by_id[manual_id] = relative
            source_text = "\n".join(cast(list[str], row["source"]))
            if row.get("state") == "current":
                expected_text = "\n".join(cast(list[str], row["translation"]))
            else:
                expected_text = source_text
            expected_tokens_by_id[manual_id] = _text_tokens(expected_text)
            for token in _text_tokens(source_text):
                source_tokens_by_file.setdefault(relative, {}).setdefault(token, set()).add(manual_id)
    missing = sorted(set(required_by_id.values()) - set(actual_files))
    findings = [
        _finding(
            "write_back_output_file_missing",
            status="confirmed_fact",
            details={"path": relative},
        )
        for relative in missing
    ]
    hits_by_id: dict[str, set[str]] = {}
    for relative, by_token in source_tokens_by_file.items():
        for token in output_tokens.get(relative, set()).intersection(by_token):
            for manual_id in by_token[token]:
                hits_by_id.setdefault(manual_id, set()).add(token)
    for manual_id, words in sorted(hits_by_id.items()):
        introduced = words - expected_tokens_by_id[manual_id]
        findings.append(
            _finding(
                "write_back_source_residual",
                manual_id=manual_id,
                details={
                    "path": required_by_id[manual_id],
                    "words": sorted(words)[:10],
                    "introduced_words": sorted(introduced)[:10],
                    "scope": "same_output_file_without_unit_recipe",
                },
            )
        )
    if rows:
        unverified.append("rpg_write_back_unit_mapping_unverified")
    return findings, unverified


def _write_back_findings(
    path: Path | None,
    rows: Sequence[Mapping[str, JsonValue]],
    manifest: Mapping[str, Mapping[str, JsonValue]] | None,
    *,
    generic_project: bool,
    engine: str | None,
    generic_expected_files: set[str] | None = None,
    strict_generic_text: bool = False,
) -> tuple[list[dict[str, JsonValue]], list[str], Path | None, dict[str, JsonValue] | None]:
    if path is None:
        return [], ["write_back_output_missing"], None, None
    root = require_directory(path, "ATT write_back 目录")
    if generic_project:
        if manifest is None:
            return [], ["generic_manifest_missing"], root, _directory_fact(root)
        findings, transformed_current, write_back_fact = _generic_write_back_findings(
            root,
            rows,
            manifest,
            expected_files=generic_expected_files,
            strict_export_text=strict_generic_text,
        )
        unverified = ["generic_write_back_text_transform_unverified"] if transformed_current else []
        return findings, unverified, root, write_back_fact
    if engine not in {"mv", "mz"}:
        fail("Survey", "RPG Maker QA 缺少有效引擎", "重新运行 survey scan/finalize")
    findings, unverified = _rpg_write_back_findings(root, rows, engine)
    return findings, unverified, root, _directory_fact(root)


def _jsonl_objects(path: Path, description: str) -> list[dict[str, JsonValue]]:
    output: list[dict[str, JsonValue]] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8-sig").splitlines(), start=1):
        if not line.strip():
            fail(str(path), f"{description} 第 {line_number} 行为空", "重新运行 inspect_nwjs_runtime.py")
        value = parse_json_text(line, f"{path} 第 {line_number} 行")
        if not isinstance(value, dict):
            fail(str(path), f"{description} 第 {line_number} 行不是 object", "重新运行运行时观察")
        output.append(dict(value))
    return output


def _runtime_rows(
    report: Mapping[str, JsonValue],
    report_root: Path,
    *,
    count_field: str,
    file_field: str,
) -> list[dict[str, JsonValue]]:
    count = report.get(count_field)
    relative = report.get(file_field)
    if (
        not isinstance(count, int)
        or isinstance(count, bool)
        or count < 0
        or not isinstance(relative, str)
        or not relative
    ):
        fail(
            str(report_root),
            f"运行时报告的 {count_field}/{file_field} 无效",
            "重新运行 inspect_nwjs_runtime.py",
        )
    actual = _jsonl_objects(
        require_file_within(report_root / Path(relative), report_root, file_field),
        file_field,
    )
    if len(actual) != count:
        fail(str(report_root), f"{count_field} 与对应 JSONL 行数不一致", "重新运行 inspect_nwjs_runtime.py")
    return actual


def _runtime_observer_ready(value: object) -> bool:
    if not isinstance(value, dict):
        return False
    typed_value = cast(dict[object, object], value)
    requirements_value = typed_value.get("hookRequirements")
    requirements = (
        cast(dict[object, object], requirements_value) if isinstance(requirements_value, dict) else {}
    )
    sequence = typed_value.get("sequence")
    expected_hooks = {
        "bitmapDrawText",
        "windowDrawText",
        "windowDrawTextEx",
        "addCommand",
        "loadFont",
        "fontManagerLoad",
        "graphicsPrintError",
        "graphicsPrintLoadingError",
    }
    return bool(
        typed_value.get("installed") is True
        and typed_value.get("requiredHooksInstalled") is True
        and typed_value.get("pageLoadFinished") is True
        and typed_value.get("pollingObserved") is True
        and typed_value.get("installationFinished") is True
        and len(requirements) == len(expected_hooks)
        and all(isinstance(key, str) for key in requirements)
        and {cast(str, key) for key in requirements} == expected_hooks
        and all(member is True for member in requirements.values())
        and isinstance(sequence, int)
        and not isinstance(sequence, bool)
        and sequence >= 0
    )


def _runtime_sequence(value: Mapping[str, JsonValue], description: str) -> int:
    sequence = value.get("sequence")
    if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence <= 0:
        fail(description, "运行时事件缺少正整数 sequence", "重新运行 inspect_nwjs_runtime.py")
    return sequence


def _runtime_scope(value: Mapping[str, JsonValue], description: str) -> tuple[str, str | None]:
    scope = value.get("observation_scope")
    if not isinstance(scope, dict):
        fail(description, "运行时事件缺少观察范围", "重新运行 inspect_nwjs_runtime.py")
    phase = scope.get("phase")
    scenario = scope.get("scenario")
    if (
        phase not in {"startup", "transition", "scenario", "observe", "trailing"}
        or (scenario is not None and not isinstance(scenario, str))
        or (phase == "scenario" and not isinstance(scenario, str))
    ):
        fail(description, "运行时事件观察范围无效", "重新运行 inspect_nwjs_runtime.py")
    return cast(str, phase), scenario


def _runtime_draw_is_english(value: Mapping[str, JsonValue]) -> bool:
    text_value = value.get("text")
    return isinstance(text_value, str) and any(
        "A" <= character <= "Z" or "a" <= character <= "z" for character in text_value
    )


def _runtime_draw_overflows(value: Mapping[str, JsonValue]) -> bool:
    geometry = value.get("geometry")
    return isinstance(geometry, dict) and any(
        geometry.get(field) is True
        for field in ("clippingOverflow", "overflowLeft", "overflowRight", "overflowBottom")
    )


def _runtime_measurement_unverified(value: Mapping[str, JsonValue]) -> bool:
    geometry = value.get("geometry")
    status = geometry.get("measurementStatus") if isinstance(geometry, dict) else None
    return isinstance(status, str) and status.startswith("unverified_")


def _runtime_font_not_loaded(value: Mapping[str, JsonValue]) -> bool:
    font = value.get("font")
    return isinstance(font, dict) and font.get("requestedFontLoaded") is False


def _scenario_has_expected_draw(
    name: str,
    action: Mapping[str, JsonValue],
    draws: Sequence[Mapping[str, JsonValue]],
) -> bool:
    if action.get("supported") is not True or not draws:
        return False
    if name == "dialogue":
        return any(
            draw.get("kind") == "Window_Base.drawTextEx"
            and isinstance(draw.get("text"), str)
            and bool(cast(str, draw["text"]).strip())
            and isinstance(draw.get("context"), str)
            and "Window_Message" in cast(str, draw["context"])
            for draw in draws
        )
    expected = {
        "title": "Scene_Title",
        "new_game": "Scene_Map",
        "menu": "Menu",
        "quest_log": action.get("sceneClass", "Quest"),
        "options": "Options",
        "save": "Save",
    }.get(name, name)
    if not isinstance(expected, str) or not expected:
        return False
    return any(
        isinstance(draw.get("text"), str)
        and bool(cast(str, draw["text"]).strip())
        and isinstance(member, str)
        and expected.casefold() in member.casefold()
        for draw in draws
        for member in (draw.get("scene"), draw.get("context"))
    )


def _runtime_findings(
    path: Path | None,
    survey: Mapping[str, JsonValue],
    baseline: Mapping[str, JsonValue],
    write_back_root: Path | None,
    *,
    generic_project: bool,
) -> tuple[list[dict[str, JsonValue]], list[str]]:
    if path is None:
        missing = ["runtime_observation_missing"]
        if generic_project:
            missing.append("generic_external_consumption_unverified")
        return [], missing
    report_path = require_file(path, "NW.js 运行时 report.json")
    report_root = report_path.parent
    report = read_json_object(report_path, "NW.js 运行时报告")
    mode = report.get("mode")
    engine = report.get("engine")
    if (
        mode not in {"smoke", "observe"}
        or engine != survey.get("engine")
        or report.get("input_confirmed_isolated_copy") is not True
        or report.get("keyboard_injection_used") is not False
    ):
        fail(
            str(report_path),
            "运行时报告模式、引擎或隔离副本事实无效",
            "使用当前项目的 inspect_nwjs_runtime.py 报告",
        )
    game_value = report.get("game_root")
    content_value = report.get("content_root")
    if not isinstance(game_value, str) or not isinstance(content_value, str):
        fail(str(report_path), "运行时报告缺少游戏根或内容根", "重新运行 inspect_nwjs_runtime.py")
    game_root = require_directory(Path(game_value), "运行时观察游戏根")
    content_root = require_directory(Path(content_value), "运行时观察内容根")
    if not content_root.is_relative_to(game_root):
        fail(str(report_path), "运行时内容根不在观察游戏根内", "重新运行 inspect_nwjs_runtime.py")
    survey_game = survey.get("game_root")
    if isinstance(survey_game, str) and Path(survey_game).resolve(strict=True) == game_root:
        fail(
            str(report_path),
            "运行时观察使用了 survey 的原游戏而不是隔离副本",
            "把 write_back 部署到可丢弃副本后重新观察",
        )
    owned_pid = report.get("owned_pid")
    listener_pid = report.get("cdp_listener_pid")
    page_target = report.get("page_target")
    observer = report.get("observer")
    if (
        not isinstance(owned_pid, int)
        or isinstance(owned_pid, bool)
        or owned_pid <= 0
        or not isinstance(listener_pid, int)
        or isinstance(listener_pid, bool)
        or listener_pid <= 0
        or not isinstance(page_target, str)
        or not page_target.startswith("file:")
        or not isinstance(observer, dict)
        or observer.get("installed") is not True
    ):
        fail(str(report_path), "运行时进程、页面或观察器身份无效", "重新运行 inspect_nwjs_runtime.py")

    events = _runtime_rows(report, report_root, count_field="event_count", file_field="events_file")
    draws = _runtime_rows(report, report_root, count_field="draw_count", file_field="draws_file")
    english = _runtime_rows(
        report, report_root, count_field="english_candidate_count", file_field="english_candidates_file"
    )
    overflow = _runtime_rows(
        report, report_root, count_field="pixel_overflow_count", file_field="pixel_overflows_file"
    )
    measurement = _runtime_rows(
        report,
        report_root,
        count_field="measurement_unverified_count",
        file_field="measurement_unverified_file",
    )
    runtime_errors = _runtime_rows(
        report,
        report_root,
        count_field="runtime_error_count",
        file_field="runtime_errors_file",
    )
    font_review = report.get("font_review")
    if not isinstance(font_review, dict):
        fail(str(report_path), "运行时报告缺少 font_review", "重新运行 inspect_nwjs_runtime.py")
    requested_font = _runtime_rows(
        cast(Mapping[str, JsonValue], font_review),
        report_root,
        count_field="requested_font_not_loaded_count",
        file_field="requested_font_not_loaded_file",
    )
    known_kinds = {
        "Bitmap.drawText",
        "Window_Base.drawText",
        "Window_Base.drawTextEx",
        "Window_Command.addCommand",
        "Graphics.loadFont",
        "FontManager.load",
    }
    draw_kinds = {"Bitmap.drawText", "Window_Base.drawText", "Window_Base.drawTextEx"}
    previous_sequence = 0
    for event in events:
        sequence = _runtime_sequence(event, str(report_path))
        if (
            sequence <= previous_sequence
            or event.get("kind") not in known_kinds
            or not isinstance(event.get("timestampMs"), (int, float))
            or isinstance(event.get("timestampMs"), bool)
            or not isinstance(event.get("text"), str)
            or not isinstance(event.get("scene"), str)
            or not isinstance(event.get("context"), str)
            or not isinstance(event.get("geometry"), dict)
            or not isinstance(event.get("font"), dict)
        ):
            fail(str(report_path), "运行时事件字段或递增顺序无效", "重新运行 inspect_nwjs_runtime.py")
        _runtime_scope(event, str(report_path))
        previous_sequence = sequence
    projected_draws = [event for event in events if event.get("kind") in draw_kinds]
    if draws != projected_draws:
        fail(str(report_path), "draws.jsonl 不是完整事件的精确绘制子集", "重新运行运行时观察")
    for draw in draws:
        _runtime_sequence(draw, str(report_path))
    projections = (
        (english, [draw for draw in draws if _runtime_draw_is_english(draw)], "英文候选"),
        (overflow, [draw for draw in draws if _runtime_draw_overflows(draw)], "像素越界"),
        (
            measurement,
            [draw for draw in draws if _runtime_measurement_unverified(draw)],
            "未验证布局测量",
        ),
        (
            requested_font,
            [event for event in events if _runtime_font_not_loaded(event)],
            "字体加载异常",
        ),
    )
    for actual_rows, expected_rows, description in projections:
        if actual_rows != expected_rows:
            fail(
                str(report_path),
                f"{description} JSONL 不是 events/draws 的精确子集",
                "重新运行 inspect_nwjs_runtime.py",
            )
    for runtime_error in runtime_errors:
        if not isinstance(runtime_error.get("kind"), str) or not isinstance(
            runtime_error.get("message"), str
        ):
            fail(str(report_path), "运行时错误事件字段无效", "重新运行 inspect_nwjs_runtime.py")
        _runtime_scope(runtime_error, str(report_path))

    last_sequence = previous_sequence
    observer_sequence = observer.get("sequence")
    if (
        not isinstance(observer_sequence, int)
        or isinstance(observer_sequence, bool)
        or observer_sequence < last_sequence
    ):
        fail(str(report_path), "观察器终态与事件序列不一致", "重新运行 inspect_nwjs_runtime.py")
    glyph_expected = any(isinstance(draw.get("text"), str) and cast(str, draw["text"]) for draw in draws)
    glyph_value = font_review.get("glyph_fallback_unverified")
    if not isinstance(glyph_value, bool) or glyph_value != glyph_expected:
        fail(str(report_path), "glyph fallback 未验证状态与实际绘制不一致", "重新运行运行时观察")
    glyph_unverified = glyph_value
    glyph_status = font_review.get("glyph_fallback_status")
    if glyph_status != ("unverified" if glyph_unverified else "not_observed"):
        fail(str(report_path), "glyph fallback 状态与运行时事实不一致", "重新运行 inspect_nwjs_runtime.py")
    startup = report.get("startup")
    scenarios = report.get("scenarios")
    unverified_scenarios = report.get("unverified_scenario_count")
    if (
        not isinstance(startup, dict)
        or startup.get("status") not in {"ready", "runtime_error", "timed_out", "process_exited"}
        or not isinstance(startup.get("wait_seconds"), (int, float))
        or isinstance(startup.get("wait_seconds"), bool)
        or not isinstance(scenarios, list)
        or not isinstance(unverified_scenarios, int)
        or isinstance(unverified_scenarios, bool)
        or unverified_scenarios
        != sum(not isinstance(item, dict) or item.get("status") != "verified" for item in scenarios)
    ):
        fail(str(report_path), "运行时启动或场景汇总不一致", "重新运行 inspect_nwjs_runtime.py")
    if mode == "smoke":
        scenario_names = [item.get("name") if isinstance(item, dict) else None for item in scenarios]
        if scenario_names != list(_RUNTIME_SMOKE_SCENARIOS):
            fail(
                str(report_path),
                "smoke 报告缺少固定场景或顺序不一致",
                "重新运行 inspect_nwjs_runtime.py smoke",
            )
    elif scenarios:
        fail(str(report_path), "observe 报告不应包含预定义场景", "重新运行 inspect_nwjs_runtime.py observe")
    previous_scenario_end = 0
    for scenario in scenarios:
        if not isinstance(scenario, dict):
            fail(str(report_path), "运行时场景不是 object", "重新运行 inspect_nwjs_runtime.py")
        screenshot = scenario.get("screenshot")
        screenshot_width = scenario.get("screenshot_width")
        screenshot_height = scenario.get("screenshot_height")
        sequence_start = scenario.get("event_sequence_start")
        sequence_end = scenario.get("event_sequence_end")
        observed_events = scenario.get("observed_events")
        if (
            not isinstance(scenario.get("name"), str)
            or scenario.get("status") not in {"verified", "unverified"}
            or not isinstance(scenario.get("evidence"), str)
            or not cast(str, scenario.get("evidence")).strip()
            or not isinstance(scenario.get("action"), dict)
            or not isinstance(sequence_start, int)
            or isinstance(sequence_start, bool)
            or not isinstance(sequence_end, int)
            or isinstance(sequence_end, bool)
            or sequence_start < previous_scenario_end
            or sequence_end < sequence_start
            or not isinstance(observed_events, int)
            or isinstance(observed_events, bool)
            or observed_events < 0
            or not isinstance(scenario.get("observed_draws"), int)
            or isinstance(scenario.get("observed_draws"), bool)
            or (screenshot is not None and not isinstance(screenshot, str))
            or not isinstance(scenario.get("observer_start"), dict)
            or not isinstance(scenario.get("observer_end"), dict)
        ):
            fail(str(report_path), "运行时场景证据结构无效", "重新运行 inspect_nwjs_runtime.py")
        name = cast(str, scenario["name"])
        observed_draws = cast(int, scenario["observed_draws"])
        observer_start = cast(dict[str, JsonValue], scenario["observer_start"])
        observer_end = cast(dict[str, JsonValue], scenario["observer_end"])
        if observed_draws < 0:
            fail(str(report_path), "运行时场景绘制计数无效", "重新运行 inspect_nwjs_runtime.py")
        interval_events = [
            event
            for event in events
            if sequence_start < _runtime_sequence(event, str(report_path)) <= sequence_end
        ]
        scoped_events = [
            event
            for event in interval_events
            if _runtime_scope(event, str(report_path)) == ("scenario", name)
        ]
        interval_draws = [event for event in interval_events if event.get("kind") in draw_kinds]
        if (
            interval_events != scoped_events
            or observed_events != len(interval_events)
            or observed_draws != len(interval_draws)
            or observer_start.get("sequence") != sequence_start
            or observer_end.get("sequence") != sequence_end
        ):
            fail(str(report_path), f"场景 {name} 的序列边界、事件范围或计数不一致", "重新运行 smoke")
        previous_scenario_end = sequence_end
        if isinstance(screenshot, str):
            screenshot_path = require_file_within(
                report_root / Path(screenshot), report_root, "运行时场景截图"
            )
            try:
                decoded_width, decoded_height = decode_png_size(screenshot_path.read_bytes())
            except ValueError:
                fail(
                    str(screenshot_path),
                    "运行时场景截图不是可解码的非空 PNG",
                    "重新运行 inspect_nwjs_runtime.py",
                )
            if screenshot_width != decoded_width or screenshot_height != decoded_height:
                fail(str(screenshot_path), "场景截图尺寸与报告不一致", "重新运行 inspect_nwjs_runtime.py")
        elif screenshot_width is not None or screenshot_height is not None:
            fail(str(report_path), "没有截图的场景不应声明像素尺寸", "重新运行 inspect_nwjs_runtime.py")
        if scenario.get("status") == "verified" and (
            not _runtime_observer_ready(observer_start)
            or not _runtime_observer_ready(observer_end)
            or not _scenario_has_expected_draw(
                name,
                cast(dict[str, JsonValue], scenario["action"]),
                interval_draws,
            )
            or observed_draws <= 0
            or not isinstance(screenshot, str)
            or not isinstance(screenshot_width, int)
            or isinstance(screenshot_width, bool)
            or screenshot_width < 64
            or not isinstance(screenshot_height, int)
            or isinstance(screenshot_height, bool)
            or screenshot_height < 64
        ):
            fail(
                str(report_path),
                f"已验证场景 {scenario.get('name')} 缺少本场景真实动作、绘制或截图证据",
                "重新运行 inspect_nwjs_runtime.py smoke",
            )
    actual_finding = bool(
        english or overflow or requested_font or runtime_errors or startup.get("status") != "ready"
    )
    has_unverified = bool(
        unverified_scenarios
        or mode == "observe"
        or not _runtime_observer_ready(observer)
        or glyph_unverified
        or measurement
    )
    expected_status = "needs_review" if actual_finding else "unverified" if has_unverified else "clean"
    if report.get("qa_status") != expected_status:
        fail(str(report_path), "qa_status 与运行时事实不一致", "重新运行 inspect_nwjs_runtime.py")

    unverified: list[str] = []
    if generic_project:
        unverified.append("generic_external_consumption_unverified")
    elif write_back_root is None:
        unverified.append("runtime_write_back_binding_missing")
    else:
        for output_file in safe_walk_files(write_back_root):
            relative = output_file.relative_to(write_back_root)
            deployed = require_file_within(game_root / relative, game_root, "隔离副本中的 WriteBack 文件")
            if deployed.read_bytes() != output_file.read_bytes():
                fail(
                    str(deployed),
                    f"隔离副本中的 {relative.as_posix()} 与实际 write_back 不一致",
                    "重新部署同一 write_back 后执行运行时观察",
                )
        survey_game_value = survey.get("game_root")
        survey_content_value = survey.get("content_root")
        baseline_files = baseline.get("files")
        if (
            not isinstance(survey_game_value, str)
            or not isinstance(survey_content_value, str)
            or not isinstance(baseline_files, list)
        ):
            fail(str(report_path), "Survey 来源范围不足以绑定运行副本", "重新执行 survey scan")
        survey_game_root = require_directory(Path(survey_game_value), "Survey 游戏根")
        survey_content_root = require_directory(Path(survey_content_value), "Survey 内容根")
        if not survey_content_root.is_relative_to(survey_game_root):
            fail(str(report_path), "Survey 内容根不在游戏根内", "重新执行 survey scan")
        for number, raw_file in enumerate(baseline_files, start=1):
            if not isinstance(raw_file, dict):
                fail(str(report_path), f"Survey baseline 第 {number} 项无效", "重新执行 survey scan")
            relative = raw_file.get("path")
            byte_count = raw_file.get("bytes")
            digest = raw_file.get("sha256")
            if (
                not isinstance(relative, str)
                or not isinstance(byte_count, int)
                or isinstance(byte_count, bool)
                or not isinstance(digest, str)
            ):
                fail(str(report_path), f"Survey baseline 第 {number} 项字段无效", "重新执行 survey scan")
            survey_source = require_file_within(
                survey_game_root / Path(relative),
                survey_game_root,
                "Survey baseline 来源",
            )
            try:
                content_relative = survey_source.relative_to(survey_content_root)
            except ValueError:
                runtime_source = require_file_within(
                    game_root / Path(relative), game_root, "隔离副本中的 Survey 来源"
                )
            else:
                runtime_source = require_file_within(
                    content_root / content_relative,
                    content_root,
                    "隔离副本中的 Survey 内容来源",
                )
            runtime_relative = runtime_source.relative_to(game_root)
            output_source = write_back_root / runtime_relative
            actual = runtime_source.read_bytes()
            if output_source.is_file():
                if actual != output_source.read_bytes():
                    fail(
                        str(runtime_source),
                        "隔离副本的 Survey 来源与实际 WriteBack 字节不一致",
                        "重新部署同一 write_back 后执行运行时观察",
                    )
            elif len(actual) != byte_count or hashlib.sha256(actual).hexdigest() != digest:
                fail(
                    str(runtime_source),
                    "隔离副本的未写回来源与 Survey 基线不一致",
                    "从同一 Survey 来源建立隔离副本并重新部署 WriteBack",
                )
    if expected_status == "needs_review":
        return [_finding("runtime_observation_issue", status="confirmed_fact")], unverified
    if expected_status == "unverified":
        unverified.append("runtime_observation_unverified")
    return [], unverified


def _scan(args: argparse.Namespace) -> int:
    started = time.perf_counter()
    translations_path = require_file(args.translations, "ATT Translation export JSONL")
    translation_export_fact = _file_fact(translations_path, "ATT Translation export JSONL")
    survey_path = cast(Path | None, args.survey)
    generic_input_path = cast(Path | None, args.generic_input)
    coverage_argument = cast(Path | None, args.coverage)
    generic_manifest_path = cast(Path | None, args.generic_manifest)
    terminology_path = cast(Path | None, args.terminology)
    write_back_path = cast(Path | None, args.write_back)
    runtime_path = cast(Path | None, args.runtime_report)
    standalone_generic = generic_input_path is not None
    if standalone_generic:
        if coverage_argument is not None:
            fail("--coverage", "standalone Generic 模式不接受 coverage", "删除 --coverage")
        if generic_manifest_path is not None:
            fail(
                "--generic-manifest",
                "standalone Generic 模式不接受 Survey 生成的 manifest",
                "删除 --generic-manifest，直接使用 --generic-input",
            )
        if runtime_path is not None:
            fail(
                "--runtime-report",
                "standalone Generic 模式不接受 RPG Maker Survey 运行报告",
                "删除 --runtime-report，并在任务中另行验证实际 Generic 消费者",
            )
    elif coverage_argument is None:
        fail("--coverage", "Survey 模式缺少 coverage", "同时提供 --survey 与 --coverage")

    inputs = [translations_path]
    survey_root = require_directory(survey_path, "survey 作业目录") if survey_path is not None else None
    generic_input_root = (
        require_directory(generic_input_path, "Generic JSONL 输入根")
        if generic_input_path is not None
        else None
    )
    coverage_path = (
        require_file(coverage_argument, "finalize coverage.json") if coverage_argument is not None else None
    )
    rules_manifest_path = (
        require_file(
            coverage_path.with_name("rules-manifest.json"),
            "同一次 finalize 生成的 rules-manifest.json",
        )
        if coverage_path is not None
        else None
    )
    inputs.extend(
        path
        for path in (survey_root, generic_input_root, coverage_path, rules_manifest_path)
        if path is not None
    )
    inputs.extend(
        path
        for path in (generic_manifest_path, terminology_path, write_back_path, runtime_path)
        if path is not None
    )
    protect_outputs([args.output], inputs=inputs, replace=args.replace)
    rows = _read_translation_export(translations_path)

    survey: dict[str, JsonValue] | None
    locations: list[dict[str, JsonValue]]
    baseline: dict[str, JsonValue] | None
    generic_tree: _GenericTree | None
    generic_expected_files: set[str] | None
    if standalone_generic:
        assert generic_input_root is not None
        survey = None
        locations = []
        baseline = None
        generic_tree = _read_generic_tree(generic_input_root, "Generic JSONL 输入根", only_jsonl=False)
        generic_recipes = _standalone_generic_recipes(generic_tree)
        generic_expected_files = set(generic_tree.files)
        coverage_complete = None
        generic_project = True
        _bind_generic_export_to_input(rows, generic_tree)
    else:
        assert survey_root is not None
        assert coverage_path is not None
        assert rules_manifest_path is not None
        survey, locations, groups, baseline = load_survey(survey_root)
        if write_back_path is not None or runtime_path is not None:
            verify_source_baseline(survey, baseline)
        rules_manifest = read_rules_manifest(rules_manifest_path)
        coverage_projection, generic_candidates, coverage_complete = _coverage_projection(
            coverage_path,
            survey,
            locations,
            groups,
            rules_manifest,
        )
        generic_tree = None
        generic_expected_files = None
        generic_recipes = _generic_manifest(generic_manifest_path)
        owner_values: set[str | None] = set()
        for row in rows:
            owner = row.get("owner")
            owner_values.add(owner if isinstance(owner, str) else None)
        if None in owner_values and len(owner_values) > 1:
            fail(
                str(translations_path),
                "RPG Maker 与 Generic 导出不能混在一个文件",
                "分别执行译后 QA",
            )
        generic_project = generic_recipes is not None or owner_values == {None}
        if generic_project and any(row.get("owner") is not None for row in rows):
            fail(
                str(translations_path),
                "Generic manifest 与 RPG Maker Translation export 不匹配",
                "使用对应项目的导出",
            )
        if generic_project:
            if generic_recipes is not None:
                _bind_generic_manifest(generic_recipes, generic_candidates, locations)
                _bind_generic_export_to_manifest(rows, generic_recipes)
        else:
            if generic_recipes is not None:
                fail(
                    str(generic_manifest_path),
                    "RPG Maker Translation export 不应附带 Generic manifest",
                    "只为对应 Generic 项目单独执行 QA",
                )
            _bind_rpg_export_to_coverage(rows, coverage_projection)

    if standalone_generic and any(row.get("owner") is not None for row in rows):
        fail(
            str(translations_path),
            "standalone Generic 输入与 RPG Maker Translation export 不匹配",
            "使用该 Generic 项目的完整 translation export",
        )
    terms = _terminology(terminology_path)
    findings = [finding for row in rows for finding in _translation_findings(row, terms)]
    if not standalone_generic:
        findings.extend(
            _survey_findings(
                locations,
                {cast(str, row["manual_id"]) for row in rows},
                check_builtin=not generic_project,
            )
        )
    # 现行 Translation export 没有项目目标语言；精确源文残留只能形成 Review，
    # 即使写回和运行证据齐全，也不能据此证明全量语言方向正确。
    unverified: list[str] = ["translation_language_pair_unbound"]
    if standalone_generic:
        unverified.extend(
            [
                "generic_external_source_mapping_unverified",
                "generic_reverse_conversion_unverified",
                "generic_actual_consumer_unverified",
            ]
        )
    elif generic_project and generic_recipes is None:
        unverified.append("generic_manifest_missing")
    if coverage_complete is False:
        unverified.append("survey_coverage_incomplete")
    write_back_findings, write_back_unverified, write_back_root, write_back_fact = _write_back_findings(
        write_back_path,
        rows,
        generic_recipes,
        generic_project=generic_project,
        engine=cast(str, survey["engine"]) if survey is not None else None,
        generic_expected_files=generic_expected_files,
        strict_generic_text=standalone_generic,
    )
    if standalone_generic:
        runtime_findings: list[dict[str, JsonValue]] = []
        runtime_unverified: list[str] = []
    else:
        assert survey is not None
        assert baseline is not None
        runtime_findings, runtime_unverified = _runtime_findings(
            runtime_path,
            survey,
            baseline,
            write_back_root,
            generic_project=generic_project,
        )
    findings.extend(write_back_findings)
    findings.extend(runtime_findings)
    unverified.extend(write_back_unverified)
    unverified.extend(runtime_unverified)
    unverified = list(dict.fromkeys(unverified))
    if generic_tree is not None:
        refreshed_tree = _read_generic_tree(
            generic_tree.root,
            "Generic JSONL 输入根",
            only_jsonl=False,
        )
        if refreshed_tree != generic_tree:
            fail(
                str(generic_tree.root),
                "Generic JSONL 输入在 QA 期间发生变化",
                "等待输入稳定后重新执行完整 QA",
            )
    if _file_fact(translations_path, "ATT Translation export JSONL") != translation_export_fact:
        fail(
            str(translations_path),
            "Translation export 在 QA 期间发生变化",
            "等待导出稳定后重新执行完整 QA",
        )
    for number, finding in enumerate(findings, start=1):
        finding["finding_id"] = f"finding-{number:06d}"
    review_groups = _group_heuristic_findings(findings)
    confirmed_ids = {
        cast(str, finding["manual_id"])
        for finding in findings
        if finding.get("analysis_status") == "confirmed_fact" and isinstance(finding.get("manual_id"), str)
    }
    revision_ids = [cast(str, row["manual_id"]) for row in rows if row["manual_id"] in confirmed_ids]
    status = "needs_review" if findings else "unverified" if unverified else "clean"
    counts = Counter(cast(str, finding["kind"]) for finding in findings)
    heuristic_findings = sum(finding.get("analysis_status") == "heuristic_review" for finding in findings)
    summary: dict[str, JsonValue] = {
        "qa_status": status,
        "translation_export": translation_export_fact,
        "write_back": write_back_fact,
        "translations": len(rows),
        "findings": len(findings),
        "heuristic_findings": heuristic_findings,
        "review_groups": len(review_groups),
        "counts": dict(sorted(counts.items())),
        "revision_ids": revision_ids,
        "unverified": unverified,
    }
    if generic_tree is not None:
        summary["generic_input"] = generic_tree.fact
    else:
        assert coverage_path is not None
        assert rules_manifest_path is not None
        assert survey is not None
        summary["coverage"] = _file_fact(coverage_path, "finalize coverage.json")
        summary["rules_manifest"] = _file_fact(rules_manifest_path, "rules-manifest.json")
        summary["survey_game_root"] = survey.get("game_root", "")
    metrics: dict[str, JsonValue] = {
        "translation_entries_scanned": len(rows),
        "survey_locations_checked": len(locations),
        "revision_ids": len(revision_ids),
        "review_groups": len(review_groups),
        "local_command_elapsed_ms": round((time.perf_counter() - started) * 1000),
        "external_request_wait_ms": 0,
    }
    atomic_write_directory(
        args.output,
        {
            "qa-summary.json": _json_text(summary),
            "findings.jsonl": _json_lines(findings),
            "review-groups.jsonl": _json_lines(review_groups),
            "agent-work-metrics.json": _json_text(metrics),
        },
        replace=args.replace,
    )
    print(
        f"译后 QA：{status}；确定问题涉及 {len(revision_ids)} 个自然 Unit，"
        f"{heuristic_findings} 项启发式结果已压缩为 {len(review_groups)} 个 Review 组。"
    )
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
    protected_inputs = [scan_root, Path(path_value)]
    generic_input_fact = summary.get("generic_input")
    if generic_input_fact is not None:
        if not isinstance(generic_input_fact, dict):
            fail(str(scan_root), "QA 摘要的 Generic 输入基线无效", "重新运行 translation_qa.py scan")
        expected_path = generic_input_fact.get("path")
        if not isinstance(expected_path, str):
            fail(str(scan_root), "QA 摘要缺少 Generic 输入路径", "重新运行 translation_qa.py scan")
        current_tree = _read_generic_tree(
            Path(expected_path), "QA 使用的 Generic JSONL 输入根", only_jsonl=False
        )
        if current_tree.fact != generic_input_fact:
            fail(expected_path, "Generic JSONL 输入在 QA 后发生变化", "重新运行 translation_qa.py scan")
        protected_inputs.append(current_tree.root)
    write_back_fact = summary.get("write_back")
    if write_back_fact is not None:
        if not isinstance(write_back_fact, dict):
            fail(str(scan_root), "QA 摘要的 WriteBack 基线无效", "重新运行 translation_qa.py scan")
        write_back_path = write_back_fact.get("path")
        if not isinstance(write_back_path, str):
            fail(str(scan_root), "QA 摘要缺少 WriteBack 路径", "重新运行 translation_qa.py scan")
        write_back_root = require_directory(Path(write_back_path), "QA 使用的 WriteBack 目录")
        if _directory_fact(write_back_root) != write_back_fact:
            fail(write_back_path, "WriteBack 输出在 QA 后发生变化", "重新运行 translation_qa.py scan")
        protected_inputs.append(write_back_root)
    protect_outputs([args.output], inputs=protected_inputs, replace=args.replace)
    selected_groups = cast(list[str], args.review_group)
    if len(selected_groups) != len(set(selected_groups)) or any(not value for value in selected_groups):
        fail("--review-group", "Review 组为空或重复", "每个 review_group_id 只传一次")
    requested_ids = {cast(str, value) for value in revision_ids}
    if selected_groups:
        group_rows = _read_jsonl_objects(scan_root / "review-groups.jsonl", "translation_qa.py scan")
        available_groups = {
            cast(str, row["review_group_id"])
            for row in group_rows
            if isinstance(row.get("review_group_id"), str)
        }
        unknown = sorted(set(selected_groups) - available_groups)
        if unknown:
            fail("--review-group", f"Review 组 {unknown[0]} 不存在", "使用当前 review-groups.jsonl 中的 ID")
        for finding in _read_jsonl_objects(scan_root / "findings.jsonl", "translation_qa.py scan"):
            if finding.get("review_group_id") not in selected_groups:
                continue
            manual_id = finding.get("manual_id")
            if isinstance(manual_id, str):
                requested_ids.add(manual_id)
    translation_rows = _read_translation_export(Path(path_value))
    rows = [
        {"manual_id": cast(str, row["manual_id"])}
        for row in translation_rows
        if row["manual_id"] in requested_ids
    ]
    atomic_write_text(args.output, _json_lines(rows), replace=args.replace)
    print(f"已输出 {len(rows)} 个自然 ID：{display_path(args.output)}")
    print("下一步：把该文件交给 att mv|mz|generic manual export --ids，取得数据库当前译文的预填 Manual。")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _scan(parsed) if parsed.command == "scan" else _manual(parsed))
