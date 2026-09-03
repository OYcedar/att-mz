#!/usr/bin/env python3
"""在 Translate 前核对来源、固定槽位和自定义 Placeholder。"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import time
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

# Skill 目录是发行资源，入口进程不得把解释器缓存写回包内。
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ManualEntry,
    ToolArgumentParser,
    atomic_write_directory,
    display_path,
    fail,
    parse_json_text,
    physical_jsonl_lines,
    protect_outputs,
    read_json_object,
    read_manual,
    read_physical_text,
    require_directory,
    require_file,
    run_cli,
    toml_string,
    validate_object_keys,
)
from att_toolbox.coverage import coverage_projection
from att_toolbox.rpg_control_codes import (
    ConsumerProfile,
    ControlContract,
    builtin_control_spans,
    is_structural_blank,
    unprotected_format_arguments,
)
from att_toolbox.survey import load_survey, read_jsonl, survey_game_root, verify_source_baseline
from att_toolbox.survey_projection import read_rules_manifest

_CUSTOM_FORMS: tuple[tuple[str, re.Pattern[str], str], ...] = (
    (
        "paired_angle_tag",
        re.compile(
            r"(?P<opening><(?P<name>[A-Za-z][A-Za-z0-9_-]*)(?:[^\S\r\n][^>\r\n]*)?>)"
            r"(?P<body>.*?)</(?P=name)>",
            re.DOTALL,
        ),
        "",
    ),
    (
        "angle_label",
        re.compile(r"<(?P<name>[A-Za-z][A-Za-z0-9_-]*):(?P<body>[^>\r\n]+)>", re.ASCII),
        "",
    ),
    (
        "backslash_bracket",
        re.compile(r"\\(?P<name>[A-Za-z][A-Za-z0-9]*)\[[^\]\r\n]*\]"),
        r"\\{name}\[[^]\r\n]*\]",
    ),
    (
        "escape_bracket",
        re.compile(r"\x1b(?P<name>[A-Za-z][A-Za-z0-9]*)\[[^\]\r\n]*\]"),
        "",
    ),
    (
        "backslash_angle",
        re.compile(r"\\(?P<name>[A-Za-z][A-Za-z0-9]*)<[^>\r\n]*>"),
        r"\\{name}<[^>\r\n]*>",
    ),
    (
        "escape_angle",
        re.compile(r"\x1b(?P<name>[A-Za-z][A-Za-z0-9]*)<[^>\r\n]*>"),
        "",
    ),
    ("mustache", re.compile(r"\{\{[^{}\r\n]+\}\}"), r"\{\{[^{}\r\n]+\}\}"),
    ("template", re.compile(r"\$\{[^{}\r\n]+\}"), r"\$\{[^{}\r\n]+\}"),
    ("percent", re.compile(r"%[A-Za-z_][A-Za-z0-9_]*%"), r"%[A-Za-z_][A-Za-z0-9_]*%"),
    ("known_simple_control", re.compile(r"(?:\\|\x1b)[Gg{}$.|!><^]"), ""),
    ("control_word", re.compile(r"(?:\\|\x1b)(?P<name>[A-Za-z][A-Za-z0-9]*)"), ""),
    ("angle_tag", re.compile(r"</?[A-Za-z][^>\r\n]*>"), r"</?[A-Za-z][^>\r\n]*>"),
)
_CROSS_LINE_FORMS: tuple[tuple[str, re.Pattern[str], str], ...] = (
    ("backslash_bracket", re.compile(r"\\[A-Za-z]+\["), "]"),
    ("backslash_angle", re.compile(r"\\[A-Za-z]+<"), ">"),
    ("mustache", re.compile(r"\{\{"), "}}"),
    ("template", re.compile(r"\$\{"), "}"),
    ("angle_tag", re.compile(r"</?[A-Za-z]"), ">"),
)
_ACTIONS = {"protect", "ignore", "unresolved"}


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(
        description="Translate 前核对当前 Extract 来源、固定槽位和自定义 Placeholder；Agent 不写正则。"
    )
    parser.add_argument("--manual", type=Path, required=True, help="当前 Extract 导出的 Manual TOML")
    parser.add_argument("--survey", type=Path, required=True, help="rpg_maker_survey scan 作业目录")
    parser.add_argument("--coverage", type=Path, required=True, help="同次 finalize 生成的 coverage.json")
    parser.add_argument("--output", type=Path, required=True, help="preflight 作业目录")
    parser.add_argument("--decisions", type=Path, help="基于首次候选输出编写的审核 JSONL")
    parser.add_argument("--replace", action="store_true", help="替换已存在的作业目录")
    return parser


def _json_text(value: JsonValue) -> str:
    return json.dumps(value, ensure_ascii=False, indent=2) + "\n"


def _json_lines(values: Sequence[Mapping[str, JsonValue]]) -> str:
    return "".join(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n" for value in values)


def _file_baseline(path: Path) -> dict[str, JsonValue]:
    source = require_file(path, path.name)
    raw = source.read_bytes()
    return {"path": str(source.resolve()), "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}


def _input_baseline(
    manual: Path,
    survey_root: Path,
    coverage: Path,
    rules_manifest: Path,
) -> dict[str, JsonValue]:
    return {
        "manual": _file_baseline(manual),
        "survey": _file_baseline(survey_root / "survey.json"),
        "locations": _file_baseline(survey_root / "locations.jsonl"),
        "review_groups": _file_baseline(survey_root / "review-groups.jsonl"),
        "source_baseline": _file_baseline(survey_root / "source-baseline.json"),
        "coverage": _file_baseline(coverage),
        "rules_manifest": _file_baseline(rules_manifest),
    }


def _load_previous_scan(
    output: Path,
    baseline: Mapping[str, JsonValue],
) -> tuple[
    list[dict[str, JsonValue]],
    list[dict[str, JsonValue]],
    dict[str, JsonValue],
]:
    root = require_directory(output, "已有 preflight 作业目录")
    previous = read_json_object(root / "preflight.json", "已有 preflight 报告", allowed_root=root)
    old = previous.get("input_baseline")
    if not isinstance(old, dict) or dict(old) != dict(baseline):
        fail(
            str(root),
            "Manual、survey 或 coverage 与首次 preflight 不同",
            "删除旧决定，对当前 Extract 重新运行不带 --decisions 的 preflight",
        )
    candidates = read_jsonl(root / "placeholder-candidates.jsonl", "首次 preflight 候选")
    fixed_structure = read_jsonl(root / "fixed-structure.jsonl", "首次 preflight 固定结构")
    artifact_sha256 = previous.get("scan_artifact_sha256")
    facts: dict[str, JsonValue] = {}
    for field in ("translation_entries", "fixed_structure_entries", "fixed_blank_slots"):
        value = previous.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            fail(str(root), f"首次 preflight 报告缺少有效 {field}", "重新运行不带 --decisions 的 preflight")
        facts[field] = value
    if (
        previous.get("candidates") != len(candidates)
        or facts["fixed_structure_entries"] != len(fixed_structure)
        or not isinstance(artifact_sha256, dict)
    ):
        fail(str(root), "首次 preflight 报告与明细不一致", "重新运行不带 --decisions 的 preflight")
    for name in ("placeholder-candidates.jsonl", "fixed-structure.jsonl"):
        expected_digest = artifact_sha256.get(name)
        actual_digest = hashlib.sha256((root / name).read_bytes()).hexdigest()
        if not isinstance(expected_digest, str) or expected_digest != actual_digest:
            fail(str(root / name), "首次 preflight 明细已被改写", "重新运行不带 --decisions 的 preflight")
    for number, candidate in enumerate(candidates, start=1):
        if candidate.get("candidate_id") != f"candidate-{number:06d}":
            fail(str(root), "首次 preflight 候选自然编号无效", "重新运行不带 --decisions 的 preflight")
    return candidates, fixed_structure, facts


def _read_decisions(path: Path) -> dict[str, dict[str, JsonValue]]:
    source = require_file(path, "preflight 审核 JSONL")
    output: dict[str, dict[str, JsonValue]] = {}
    for line_number, line in physical_jsonl_lines(read_physical_text(source), str(source)):
        if not line.strip():
            fail(str(source), f"第 {line_number} 行为空", "删除空行")
        value = parse_json_text(line, f"{source} 第 {line_number} 行")
        if not isinstance(value, dict):
            fail(str(source), f"第 {line_number} 行不是 object", "每行写一个审核决定 object")
        row = dict(value)
        validate_object_keys(
            row,
            f"{source} 第 {line_number} 行",
            {"target", "decision", "protection", "reason", "evidence"},
        )
        target = row.get("target")
        action = row.get("decision")
        if not isinstance(target, str) or not target.startswith("preflight:") or target in output:
            fail(
                str(source), f"第 {line_number} 行 target 无效或重复", "使用候选中的 preflight:<candidate_id>"
            )
        if not isinstance(action, str) or action not in _ACTIONS:
            fail(str(source), f"{target} 的 decision 无效", "使用 protect、ignore 或 unresolved")
        output[target] = row
    return output


def _overlaps(start: int, end: int, spans: Sequence[tuple[int, int, str]]) -> bool:
    return any(start < other_end and other_start < end for other_start, other_end, _ in spans)


def _candidate_patterns(
    kind: str,
    match: re.Match[str],
    template: str,
) -> tuple[str, str | None]:
    whole = template if template else re.escape(match.group(0))
    name = match.groupdict().get("name")
    if name is not None:
        whole = whole.format(name=re.escape(name))
    if kind == "paired_angle_tag":
        opening = cast(str, match.groupdict()["opening"])
        closing = f"</{cast(str, name)}>"
        return re.escape(match.group(0)), rf"{re.escape(opening)}(?P<text>(?s:.*?)){re.escape(closing)}"
    if kind == "angle_label":
        prefix = f"<{cast(str, name)}:"
        return re.escape(match.group(0)), rf"{re.escape(prefix)}(?P<text>[^>\r\n]+)>"
    if kind == "backslash_bracket":
        return whole, rf"\\{re.escape(cast(str, name))}\[(?P<text>[^\]\r\n]*)\]"
    if kind == "escape_bracket":
        return whole, rf"{re.escape(chr(27))}{re.escape(cast(str, name))}\[(?P<text>[^\]\r\n]*)\]"
    if kind == "backslash_angle":
        return whole, rf"\\{re.escape(cast(str, name))}<(?P<text>[^>\r\n]*)>"
    if kind == "escape_angle":
        return whole, rf"{re.escape(chr(27))}{re.escape(cast(str, name))}<(?P<text>[^>\r\n]*)>"
    if kind == "mustache":
        return whole, r"\{\{(?P<text>[^{}\r\n]+)\}\}"
    if kind == "template":
        return whole, r"\$\{(?P<text>[^{}\r\n]+)\}"
    if kind == "percent":
        return whole, r"%(?P<text>[A-Za-z_][A-Za-z0-9_]*)%"
    return whole, None


def _starts_at_paired_introducer(text: str, start: int) -> bool:
    preceding = 0
    position = start - 1
    while position >= 0 and text[position] in {"\\", "\x1b"}:
        preceding += 1
        position -= 1
    return preceding % 2 == 1


def _control_contract(context: Mapping[str, JsonValue]) -> ControlContract:
    raw = context.get("control_contract")
    if raw is None:
        return ControlContract("plain_text")
    if not isinstance(raw, dict):
        fail("survey locations", "control_contract 不是 object", "对当前游戏重新运行 survey scan")
    consumer = raw.get("consumer")
    if not isinstance(consumer, str) or consumer not in {"plain_text", "extended_text", "message_text"}:
        fail("survey locations", "control_contract.consumer 无效", "对当前游戏重新运行 survey scan")
    raw_arity = raw.get("format_arity")
    if raw_arity is not None and (
        not isinstance(raw_arity, int) or isinstance(raw_arity, bool) or raw_arity < 0
    ):
        fail("survey locations", "control_contract.format_arity 无效", "对当前游戏重新运行 survey scan")
    return ControlContract(cast(ConsumerProfile, consumer), raw_arity)


def _cross_line_findings(entry: ManualEntry) -> list[tuple[str, str, int, int]]:
    text = "\n".join(entry.source)
    output: list[tuple[str, str, int, int]] = []
    for kind, opener, closer in _CROSS_LINE_FORMS:
        for match in opener.finditer(text):
            close_at = text.find(closer, match.end())
            if close_at < 0 or "\n" not in text[match.end() : close_at]:
                continue
            output.append(
                (
                    kind,
                    match.group(0),
                    text.count("\n", 0, match.start()) + 1,
                    text.count("\n", 0, close_at) + 1,
                )
            )
    return output


def _physical_separator_offsets(lines: Sequence[str]) -> tuple[int, ...]:
    """返回把 Lines 内容槽用 LF 拼接后，各槽间分隔符的字符位置。"""

    offsets: list[int] = []
    position = 0
    for line in lines[:-1]:
        position += len(line)
        offsets.append(position)
        position += 1
    return tuple(offsets)


def _crosses_physical_slot(start: int, end: int, separators: Sequence[int]) -> bool:
    return any(start <= separator < end for separator in separators)


def _coverage_context(
    manual_entries: Sequence[ManualEntry],
    survey: Mapping[str, JsonValue],
    projection: Mapping[str, Mapping[str, JsonValue]],
) -> tuple[str, dict[str, str], list[dict[str, JsonValue]]]:
    engine = survey.get("engine")
    if not isinstance(engine, str) or engine not in {"mv", "mz"}:
        fail("survey.json", "引擎无效", "使用当前 survey scan 重新生成调查")

    entries_by_id = {entry.readable_id: entry for entry in manual_entries}
    if len(entries_by_id) != len(manual_entries) or set(entries_by_id) != set(projection):
        missing = sorted(set(projection) - set(entries_by_id))
        unexpected = sorted(set(entries_by_id) - set(projection))
        fail(
            "Manual/coverage",
            f"当前 Extract 所有权集合不一致：缺少 {len(missing)} 项，意外 {len(unexpected)} 项",
            "重新导出当前 Manual 和所有权并完成 audit",
        )

    owners: dict[str, str] = {}
    contexts: list[dict[str, JsonValue]] = []
    for entry in manual_entries:
        location = projection[entry.readable_id]
        owner = location.get("owner")
        if not isinstance(owner, str) or owner not in {"builtin", "rules"}:
            fail(entry.readable_id, "coverage 投影 owner 无效", "重新运行 survey finalize")
        if location.get("source_text") != "\n".join(entry.source):
            fail(
                entry.readable_id,
                "Manual 原文不能映射回 finalize 的 Unit 投影",
                "对当前游戏重新 scan、finalize、Extract 和 audit",
            )
        control_contract = location.get("control_contract")
        content_kind = location.get("content_kind")
        if not isinstance(control_contract, dict):
            fail(
                entry.readable_id,
                "当前 Unit 投影缺少消费者契约",
                "对当前游戏重新运行 survey scan、finalize、Extract 和 audit",
            )
        if content_kind not in {"value", "lines"}:
            fail(
                entry.readable_id,
                "当前 Unit 投影缺少 Value/Lines 内容结构",
                "对当前游戏重新运行 survey scan、finalize、Extract 和 audit",
            )
        owners[entry.readable_id] = owner
        contexts.append(
            {
                "manual_id": entry.readable_id,
                "owner": owner,
                "candidate_id": cast(str, location.get("candidate_id", "")),
                "source": cast(str, location.get("source", "")),
                "review_group_id": cast(str, location.get("review_group_id", "")),
                "control_contract": control_contract,
                "content_kind": cast(str, content_kind),
            }
        )
    return engine, owners, contexts


def _scan(
    engine: str,
    entries: Sequence[ManualEntry],
    owners: Mapping[str, str],
    contexts: Mapping[str, Mapping[str, JsonValue]],
) -> tuple[
    list[dict[str, JsonValue]],
    list[dict[str, JsonValue]],
    dict[str, JsonValue],
]:
    grouped: dict[tuple[str, ...], dict[str, JsonValue]] = {}
    cross_line: list[dict[str, JsonValue]] = []
    fixed_structure: list[dict[str, JsonValue]] = []

    def add_candidate(
        entry: ManualEntry,
        context: Mapping[str, JsonValue],
        *,
        form: str,
        whole_pattern: str | None,
        shell_pattern: str | None,
        observed: str,
        contract_name: str,
        format_argument_option: bool = False,
    ) -> None:
        relation = context.get("review_group_id")
        relation_key = relation if isinstance(relation, str) and relation else f"manual:{entry.readable_id}"
        raw_contract = context.get("control_contract")
        contract_key = json.dumps(raw_contract, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        key = (
            relation_key,
            contract_key,
            "placeholder_shape",
            form,
            whole_pattern or "",
            shell_pattern or "",
            contract_name,
            "format" if format_argument_option else "preserve",
        )
        options: list[dict[str, JsonValue]] = []
        if whole_pattern is not None:
            options.append(
                {
                    "protection": "whole_protocol",
                    "rule": {"ids": [], "order": "preserve", "pattern": whole_pattern},
                }
            )
            if format_argument_option:
                options.append(
                    {
                        "protection": "format_arguments",
                        "rule": {
                            "ids": [],
                            "order": "reorder_within_slot",
                            "pattern": whole_pattern,
                        },
                    }
                )
        if shell_pattern is not None:
            options.append(
                {
                    "protection": "shell_with_text",
                    "rule": {"ids": [], "order": "preserve", "pattern": shell_pattern},
                }
            )
        candidate = grouped.setdefault(
            key,
            {
                "kind": "placeholder_shape",
                "form": form,
                "analysis_status": "heuristic_review",
                "contract": contract_name,
                "rule_options": options,
                "occurrences": 0,
                "owners": [],
                "examples": [],
            },
        )
        candidate["occurrences"] = cast(int, candidate["occurrences"]) + 1
        candidate_owners = cast(list[JsonValue], candidate["owners"])
        owner = owners[entry.readable_id]
        if owner not in candidate_owners:
            candidate_owners.append(owner)
        for option in cast(list[dict[str, JsonValue]], candidate["rule_options"]):
            rule = cast(dict[str, JsonValue], option["rule"])
            ids = cast(list[JsonValue], rule["ids"])
            if entry.readable_id not in ids:
                ids.append(entry.readable_id)
        examples = cast(list[JsonValue], candidate["examples"])
        if len(examples) < 5 and not any(
            isinstance(value, dict) and value.get("manual_id") == entry.readable_id for value in examples
        ):
            examples.append({"manual_id": entry.readable_id, "observed_form": observed})

    for entry in entries:
        context = contexts[entry.readable_id]
        contract = _control_contract(context)
        scan_texts = (
            (
                "\n".join(entry.source),
                (_physical_separator_offsets(entry.source) if context.get("content_kind") == "lines" else ()),
            ),
        )
        for text, separators in scan_texts:
            builtin_spans = builtin_control_spans(engine, text, contract)
            claimed_review_spans: list[tuple[int, int, str]] = []
            for position, character in enumerate(text):
                if character != "\f" or _overlaps(position, position + 1, builtin_spans):
                    continue
                add_candidate(
                    entry,
                    context,
                    form="message_page_break",
                    whole_pattern=re.escape(character),
                    shell_pattern=None,
                    observed=character,
                    contract_name="message_consumer_not_confirmed",
                )
                claimed_review_spans.append((position, position + 1, "message_page_break"))
            for _start, _end, observed, reason in unprotected_format_arguments(text, contract):
                add_candidate(
                    entry,
                    context,
                    form="percent_number",
                    whole_pattern=(r"%[0-9]+" if reason != "invalid_source_format_argument" else None),
                    shell_pattern=None,
                    observed=observed,
                    contract_name=reason,
                    format_argument_option=reason != "invalid_source_format_argument",
                )
            for kind, expression, template in _CUSTOM_FORMS:
                for match in expression.finditer(text):
                    if match.group(0)[0] in {"\\", "\x1b"} and _starts_at_paired_introducer(
                        text, match.start()
                    ):
                        continue
                    whole_pattern, shell_pattern = _candidate_patterns(kind, match, template)
                    if kind == "paired_angle_tag" and _crosses_physical_slot(
                        match.start(), match.end(), separators
                    ):
                        whole_pattern = None
                    if shell_pattern is not None:
                        if kind in {"paired_angle_tag", "angle_label"}:
                            body_start, body_end = match.span("body")
                            protected_spans = [
                                (match.start(), body_start, kind),
                                (body_end, match.end(), kind),
                            ]
                        else:
                            delimiter_lengths = {
                                "backslash_bracket": (match.group(0).find("[") + 1, 1),
                                "escape_bracket": (match.group(0).find("[") + 1, 1),
                                "backslash_angle": (match.group(0).find("<") + 1, 1),
                                "escape_angle": (match.group(0).find("<") + 1, 1),
                                "mustache": (2, 2),
                                "template": (2, 1),
                                "percent": (1, 1),
                            }
                            prefix_length, suffix_length = delimiter_lengths[kind]
                            protected_spans = [
                                (match.start(), match.start() + prefix_length, kind),
                                (match.end() - suffix_length, match.end(), kind),
                            ]
                    else:
                        protected_spans = [(match.start(), match.end(), kind)]
                    if any(
                        _overlaps(start, end, builtin_spans) or _overlaps(start, end, claimed_review_spans)
                        for start, end, _ in protected_spans
                    ):
                        continue
                    if _overlaps(match.start(), match.end(), builtin_spans):
                        whole_pattern = None
                    add_candidate(
                        entry,
                        context,
                        form=kind,
                        whole_pattern=whole_pattern,
                        shell_pattern=shell_pattern,
                        observed=match.group(0),
                        contract_name="consumer_and_projection_confirmation_required",
                    )
                    claimed_review_spans.extend(protected_spans)
            position = 0
            while position < len(text):
                if text[position] not in {"\\", "\x1b"}:
                    position += 1
                    continue
                paired = position + 1 < len(text) and text[position + 1] in {"\\", "\x1b"}
                end = position + (2 if paired else 1)
                occupied = _overlaps(position, end, builtin_spans) or _overlaps(
                    position, end, claimed_review_spans
                )
                if not occupied and (paired or contract.consumer != "plain_text"):
                    observed_end = end if paired or end == len(text) else end + 1
                    observed = text[position:observed_end]
                    add_candidate(
                        entry,
                        context,
                        form="literal_introducer_pair" if paired else "unknown_escape_introducer",
                        whole_pattern=re.escape(observed),
                        shell_pattern=None,
                        observed=observed,
                        contract_name="consumer_confirmation_required",
                    )
                    claimed_review_spans.append((position, observed_end, "escape_introducer"))
                position = end
        cross_line_findings = _cross_line_findings(entry) if entry.translation_type == "free" else []
        for kind, opening, start_line, end_line in cross_line_findings:
            cross_line.append(
                {
                    "kind": "cross_line_structure",
                    "form": kind,
                    "analysis_status": "heuristic_review",
                    "contract": "consumer_confirmation_required",
                    "manual_id": entry.readable_id,
                    "opening": opening,
                    "start_line": start_line,
                    "end_line": end_line,
                    "rule_options": [],
                }
            )
        if entry.translation_type == "fixed":
            blank_indexes = [index for index, line in enumerate(entry.source) if is_structural_blank(line)]
            if blank_indexes:
                fixed_structure.append(
                    {
                        "kind": "fixed_blank_slots",
                        "manual_id": entry.readable_id,
                        "slot_count": len(entry.source),
                        "blank_slot_indexes": blank_indexes,
                        "status": "proven_structure",
                    }
                )
    candidates = [grouped[key] for key in sorted(grouped)] + cross_line
    for number, candidate in enumerate(candidates, start=1):
        candidate["candidate_id"] = f"candidate-{number:06d}"
    facts: dict[str, JsonValue] = {
        "translation_entries": len(entries),
        "fixed_structure_entries": len(fixed_structure),
        "fixed_blank_slots": sum(
            len(cast(list[JsonValue], item["blank_slot_indexes"])) for item in fixed_structure
        ),
    }
    return candidates, fixed_structure, facts


def _apply_decisions(
    candidates: list[dict[str, JsonValue]], decisions: Mapping[str, Mapping[str, JsonValue]]
) -> tuple[list[dict[str, JsonValue]], list[dict[str, JsonValue]], list[str]]:
    rules: list[dict[str, JsonValue]] = []
    resolutions: list[dict[str, JsonValue]] = []
    unresolved: list[str] = []
    valid_targets = {f"preflight:{candidate['candidate_id']}" for candidate in candidates}
    unexpected = sorted(set(decisions) - valid_targets)
    if unexpected:
        fail("preflight 审核 JSONL", f"target 不存在：{unexpected[0]}", "只引用当前候选中的自然 candidate_id")
    for candidate in candidates:
        candidate_id = cast(str, candidate["candidate_id"])
        target = f"preflight:{candidate_id}"
        decision = decisions.get(target)
        if decision is None:
            unresolved.append(target)
            continue
        action = cast(str, decision["decision"])
        evidence = decision.get("evidence")
        reason = decision.get("reason")
        if action in {"protect", "ignore"} and (not isinstance(evidence, str) or not evidence.strip()):
            unresolved.append(target)
            continue
        if action == "protect":
            protection = decision.get("protection")
            raw_options = candidate.get("rule_options")
            options = raw_options if isinstance(raw_options, list) else []
            selected = [
                option
                for option in options
                if isinstance(option, dict) and option.get("protection") == protection
            ]
            if candidate.get("kind") != "placeholder_shape" or len(selected) != 1:
                unresolved.append(target)
            else:
                selected_rule = selected[0].get("rule")
                if not isinstance(selected_rule, dict):
                    unresolved.append(target)
                else:
                    rules.append(dict(selected_rule))
        elif action == "unresolved":
            unresolved.append(target)
        resolutions.append(
            {
                "target": target,
                "decision": action,
                "status": (
                    "confirmed_but_not_materialized"
                    if action == "protect"
                    and not any(
                        isinstance(option, dict) and option.get("protection") == decision.get("protection")
                        for option in cast(list[JsonValue], candidate.get("rule_options", []))
                    )
                    else "confirmed_by_agent"
                    if action in {"protect", "ignore"}
                    else "unresolved"
                ),
                "reason": reason if isinstance(reason, str) else "",
                "evidence": evidence if isinstance(evidence, str) else "",
                **(
                    {"protection": cast(str, decision["protection"])}
                    if isinstance(decision.get("protection"), str)
                    else {}
                ),
            }
        )
    return rules, resolutions, unresolved


def _verify_generated_rules(
    rules: Sequence[Mapping[str, JsonValue]],
    entries: Sequence[ManualEntry],
    content_kinds: Mapping[str, str],
) -> None:
    entries_by_id = {entry.readable_id: entry for entry in entries}
    if set(content_kinds) != set(entries_by_id) or any(
        kind not in {"value", "lines"} for kind in content_kinds.values()
    ):
        fail("Placeholder Rules", "Unit 内容结构与 Manual 不一致", "重新运行当前 preflight")
    compiled: list[tuple[int, frozenset[str], re.Pattern[str]]] = []
    for number, rule in enumerate(rules, start=1):
        pattern = rule.get("pattern")
        raw_ids = rule.get("ids")
        order = rule.get("order")
        if (
            not isinstance(pattern, str)
            or not isinstance(raw_ids, list)
            or not raw_ids
            or not all(isinstance(value, str) for value in raw_ids)
            or not isinstance(order, str)
            or order not in {"preserve", "reorder_within_slot"}
        ):
            fail("Placeholder Rules", f"第 {number} 条缺少有效 ids/order/pattern", "重新运行 preflight")
        ids = frozenset(cast(list[str], raw_ids))
        if len(ids) != len(raw_ids):
            fail("Placeholder Rules", f"第 {number} 条 ids 重复", "报告工具生成规则问题")
        missing = sorted(ids - entries_by_id.keys())
        if missing:
            fail("Placeholder Rules", f"第 {number} 条引用不存在的 ID：{missing[0]}", "重新运行 preflight")
        try:
            compiled.append((number, ids, re.compile(pattern)))
        except re.error as error:
            fail("Placeholder Rules", f"第 {number} 条生成规则无法编译：{error}", "报告工具生成规则问题")
    for entry in entries:
        text = "\n".join(entry.source)
        separators = (
            _physical_separator_offsets(entry.source) if content_kinds[entry.readable_id] == "lines" else ()
        )
        occupied: list[tuple[int, int, str]] = []
        for number, ids, pattern in compiled:
            if entry.readable_id not in ids:
                continue
            for match in pattern.finditer(text):
                if match.start() == match.end():
                    fail(
                        "Placeholder Rules",
                        f"第 {number} 条在 {entry.readable_id} 产生空匹配",
                        "报告工具生成规则问题",
                    )
                if "text" in pattern.groupindex:
                    text_span = match.span("text")
                    if text_span == (-1, -1):
                        fail(
                            "Placeholder Rules",
                            f"第 {number} 条在 {entry.readable_id} 缺少 text 捕获",
                            "报告工具生成规则问题",
                        )
                    protected_spans = [
                        (match.start(), text_span[0]),
                        (text_span[1], match.end()),
                    ]
                else:
                    protected_spans = [(match.start(), match.end())]
                for protected_start, protected_end in protected_spans:
                    if protected_start == protected_end:
                        continue
                    if _crosses_physical_slot(protected_start, protected_end, separators):
                        fail(
                            "Placeholder Rules",
                            f"第 {number} 条在 {entry.readable_id} 的实际保护范围跨越物理 source 槽",
                            "选择只保护单个 source 槽的 whole 规则，或使用前后壳均位于单槽的 text 捕获",
                        )
                    conflict = next(
                        (
                            label
                            for start, end, label in occupied
                            if protected_start < end and start < protected_end
                        ),
                        None,
                    )
                    if conflict is not None:
                        fail(
                            "Placeholder Rules",
                            f"第 {number} 条在 {entry.readable_id} 与 {conflict} 实际保护范围重叠",
                            "把冲突候选保持 unresolved；不要交给 ATT 运行时才拒绝",
                        )
                    occupied.append((protected_start, protected_end, f"规则 {number}"))


def _rules_toml(rules: Sequence[Mapping[str, JsonValue]]) -> str:
    output: list[str] = []
    for rule in rules:
        ids = cast(list[str], rule["ids"])
        id_list = ", ".join(toml_string(value) for value in ids)
        output.append(
            f"[[rule]]\nids = [{id_list}]\n"
            f"order = {toml_string(cast(str, rule['order']))}\n"
            f"pattern = {toml_string(cast(str, rule['pattern']))}\n\n"
        )
    return "".join(output) if output else "rule = []\n"


def _run(args: argparse.Namespace) -> int:
    started = time.perf_counter()
    manual_path = require_file(args.manual, "Manual TOML")
    survey_root = require_directory(args.survey, "survey 作业目录")
    coverage_path = require_file(args.coverage, "coverage.json")
    rules_manifest_path = require_file(
        coverage_path.with_name("rules-manifest.json"),
        "同一次 finalize 生成的 rules-manifest.json",
    )
    decisions_path = cast(Path | None, args.decisions)
    survey, locations, groups, source_baseline = load_survey(survey_root)
    game_root = survey_game_root(survey)
    verify_source_baseline(survey, source_baseline)
    rules_manifest = read_rules_manifest(rules_manifest_path)
    rpg_projection, _generic_candidates, coverage_complete, _generic_plans = coverage_projection(
        coverage_path,
        survey,
        locations,
        groups,
        rules_manifest,
    )
    baseline = _input_baseline(manual_path, survey_root, coverage_path, rules_manifest_path)
    previous_scan = None
    if decisions_path is not None:
        previous_scan = _load_previous_scan(args.output, baseline)
    protect_outputs(
        [args.output],
        inputs=[
            manual_path,
            survey_root,
            coverage_path,
            rules_manifest_path,
            game_root,
            *([decisions_path] if decisions_path is not None else []),
        ],
        forbidden_roots=[game_root],
        replace=args.replace,
    )
    entries = read_manual(manual_path)
    engine, owners, contexts = _coverage_context(entries, survey, rpg_projection)
    contexts_by_id = {cast(str, context["manual_id"]): context for context in contexts}
    if previous_scan is None:
        candidates, fixed_structure, facts = _scan(engine, entries, owners, contexts_by_id)
    else:
        candidates, fixed_structure, facts = previous_scan
        if facts["translation_entries"] != len(entries):
            fail(
                str(args.output),
                "首次 preflight 报告与当前 Manual 条目数不一致",
                "重新运行不带 --decisions 的 preflight",
            )
    decisions = _read_decisions(decisions_path) if decisions_path is not None else {}
    reviewed_rules, resolutions, unresolved = _apply_decisions(candidates, decisions)
    rules = reviewed_rules
    _verify_generated_rules(
        rules,
        entries,
        {manual_id: cast(str, context["content_kind"]) for manual_id, context in contexts_by_id.items()},
    )
    complete = coverage_complete and not unresolved
    candidates_text = _json_lines(candidates)
    fixed_structure_text = _json_lines(fixed_structure)
    report: dict[str, JsonValue] = {
        "complete": complete,
        "coverage_complete": coverage_complete,
        "input_baseline": baseline,
        **facts,
        "candidates": len(candidates),
        "scan_artifact_sha256": {
            "placeholder-candidates.jsonl": hashlib.sha256(candidates_text.encode("utf-8")).hexdigest(),
            "fixed-structure.jsonl": hashlib.sha256(fixed_structure_text.encode("utf-8")).hexdigest(),
        },
        "generated_rules": len(rules),
        "unresolved": unresolved,
        "resolutions": resolutions,
        "extract_contexts": contexts,
    }
    metrics: dict[str, JsonValue] = {
        "manual_entries_scanned": len(entries),
        "candidate_groups": len(candidates),
        "structural_facts": len(fixed_structure),
        "explicit_decisions_required": len(candidates),
        "explicit_decisions_received": len(decisions),
        "handwritten_rule_objects_required": 0,
        "local_command_elapsed_ms": round((time.perf_counter() - started) * 1000),
        "external_request_wait_ms": 0,
    }
    atomic_write_directory(
        args.output,
        {
            "preflight.json": _json_text(report),
            "placeholder-candidates.jsonl": candidates_text,
            "fixed-structure.jsonl": fixed_structure_text,
            "placeholder-rules.toml": _rules_toml(rules),
            "agent-work-metrics.json": _json_text(metrics),
        },
        replace=args.replace,
    )
    state = "完整" if complete else "仍有需审核或未覆盖项；Translate 可运行，但不能宣称译前检查完整"
    print(
        f"译前检查：{state}。候选 {len(candidates)} 组，固定空槽 {facts['fixed_blank_slots']} 个，已生成规则 {len(rules)} 条。"
    )
    print(f"检查目录：{display_path(args.output)}")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _run(parsed))
