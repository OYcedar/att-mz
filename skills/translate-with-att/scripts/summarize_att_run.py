#!/usr/bin/env python3
"""从 ATT 公开项目 JSONL 与任务记录汇总实际运行结果。"""

from __future__ import annotations

import argparse
import re
import sys
import unicodedata
from collections import Counter
from collections.abc import Mapping
from datetime import datetime
from pathlib import Path
from typing import cast

# Skill 目录是发行资源，入口进程不得把解释器缓存写回包内。
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    display_path,
    fail,
    parse_json_text,
    physical_jsonl_lines,
    protect_outputs,
    read_physical_text,
    require_directory,
    require_file,
    run_cli,
    safe_walk_files,
    write_json,
)

_MAX_U64 = (1 << 64) - 1
_PHASES = {
    "check_project",
    "scan_source",
    "prepare_candidate",
    "update_database",
    "publish",
    "builtin",
    "builtin_documents",
    "builtin_work_units",
    "builtin_commit",
    "rules",
    "rules_documents",
    "rules_matches",
    "rules_commit",
    "lua",
    "planning",
    "confirmed_tasks",
    "read_assets",
    "plan_rpg_maker_write_back",
    "rewrite_documents",
    "validate_candidate",
}
_TASK_OUTCOMES = {
    "complete",
    "partial",
    "unavailable",
    "failed",
    "not_committed_after_earlier_failure",
    "cancelled",
}
_TASK_COUNTER_FIELDS = {
    "planned",
    "started",
    "complete",
    "partial",
    "unavailable",
    "failed",
    "cancelled",
    "not_started",
}
_GENERIC_SUMMARY_FIELDS = {
    "planned_units",
    "remaining_units",
    "rejected_units",
    "cleared_units",
    "reused_units",
    "accepted_units",
    "written_units",
    "conflicted_units",
    "response_problems",
    "recoverable_request_exhaustions",
    "request_admission_stopped",
}
_RPG_MAKER_SUMMARY_FIELDS = {
    "accepted_decisions",
    "written_locations",
    "remaining_decisions",
    "remaining_locations",
    "rejected_locations",
    "protocol_diagnostics",
    "recoverable_request_exhaustions",
    "request_admission_stopped",
    "retained",
    "invalidated",
    "not_applicable",
    "reused",
}
_DIAGNOSTIC_EVENTS = {
    "diagnostic.run",
    "diagnostic.run_plan",
    "diagnostic.translation_task",
    "diagnostic.extract",
    "diagnostic.write_back",
    "diagnostic.publication",
    "diagnostic.task_record",
    "diagnostic.project_log",
}
_EVENTS = {
    "run.started",
    "run.cancel_requested",
    "phase.started",
    "phase.completed",
    "phase.stopped",
    "run_plan.resolved",
    "run_plan.finalized",
    "task.started",
    "task.finished",
    "translation.finished",
    "retry.summary",
    "publication.started",
    "publication.finished",
    "lua.print",
    *_DIAGNOSTIC_EVENTS,
    "performance.counters",
    "run.finished",
}
_GENERIC_PUBLICATION_FIELDS = {
    "files",
    "translated_units",
    "retained_source_units",
}
_RPG_MAKER_PUBLICATION_FIELDS = {
    "translated_units",
    "original_units",
}
_NATURAL_SEQUENCE = r"(?:[0-9]{6}|[1-9][0-9]{6,})"
_RUN_DIRECTORY = re.compile(rf"run-(?P<number>{_NATURAL_SEQUENCE})", re.ASCII)
_TASK_RECORD = re.compile(rf"task-(?P<number>{_NATURAL_SEQUENCE})\.md", re.ASCII)
_PROJECT_LOG_LOCALES = {"ar", "zh-Hans", "zh-Hant", "en", "fr", "ru", "es", "ja", "ko", "vi"}
_PROJECT_LOG_ENGINES = {"generic", "rpg_maker_mv", "rpg_maker_mz"}
_PROJECT_LOG_COMMANDS = {"init", "extract", "builtin", "rules", "translate", "write_back", "lua"}
_BIDI_CONTROLS = {
    "\u061c",
    "\u200e",
    "\u200f",
    "\u202a",
    "\u202b",
    "\u202c",
    "\u202d",
    "\u202e",
    "\u2066",
    "\u2067",
    "\u2068",
    "\u2069",
}
_MAX_PROVIDER_NAME_BYTES = 128


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="汇总阶段耗时、任务终态、规划诊断和 Translate 最终剩余量。")
    parser.add_argument("--log", type=Path, action="append", required=True, help="可重复传入 run-*.jsonl")
    parser.add_argument("--task-records", type=Path, help="可选 task-records 目录")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replace", action="store_true")
    return parser


def _safe_public_text(value: JsonValue, object_name: str, *, require_non_blank: bool = True) -> str:
    if not isinstance(value, str) or (require_non_blank and not value.strip()):
        fail(object_name, "值不是有效安全 string", "恢复 ATT 原始项目日志")
    previous_was_space = False
    for character in value:
        if (
            character in _BIDI_CONTROLS
            or character in {"\u2028", "\u2029"}
            or unicodedata.category(character) == "Cc"
        ):
            fail(object_name, "值包含项目日志不会保存的控制字符", "恢复 ATT 原始项目日志")
        if character.isspace() and previous_was_space:
            fail(object_name, "值包含项目日志会合并的连续空白", "恢复 ATT 原始项目日志")
        previous_was_space = character.isspace()
    return value


def _project_log_context(value: JsonValue, object_name: str) -> dict[str, JsonValue]:
    context = _exact_object(value, object_name, {"locale", "engine", "project", "command"})
    if context["locale"] not in _PROJECT_LOG_LOCALES:
        fail(object_name, "locale 不是当前项目日志枚举", "恢复 ATT 原始项目日志")
    if context["engine"] not in _PROJECT_LOG_ENGINES:
        fail(object_name, "engine 不是当前项目日志枚举", "恢复 ATT 原始项目日志")
    if context["command"] not in _PROJECT_LOG_COMMANDS:
        fail(object_name, "command 不是当前项目日志枚举", "恢复 ATT 原始项目日志")
    _safe_public_text(context["project"], f"{object_name}.project")
    return context


def _read_records(path: Path) -> list[dict[str, JsonValue]]:
    source = require_file(path, "ATT 项目 JSONL")
    result: list[dict[str, JsonValue]] = []
    expected_sequence = 1
    expected_run_id: str | None = None
    expected_context: dict[str, JsonValue] | None = None
    expected_fields = {
        "timestamp",
        "sequence",
        "run_id",
        "level",
        "event",
        "context",
        "payload",
        "message",
    }
    for line_number, line in physical_jsonl_lines(read_physical_text(source), str(source)):
        if not line.strip():
            fail(str(source), f"第 {line_number} 行为空", "使用 ATT 完整写入的 JSONL 日志")
        raw = parse_json_text(line, f"{source}:第 {line_number} 行")
        if not isinstance(raw, dict):
            fail(str(source), f"第 {line_number} 行根值不是 object", "使用 ATT 项目日志")
        record = raw
        if set(record) != expected_fields:
            fail(
                str(source),
                f"第 {line_number} 行顶层字段不符合项目日志当前格式",
                "保留固定的 timestamp、sequence、run_id、level、event、context、payload、message",
            )
        sequence = record.get("sequence")
        if isinstance(sequence, bool) or sequence != expected_sequence:
            fail(
                str(source),
                f"第 {line_number} 行 sequence 为 {sequence}，期望 {expected_sequence}",
                "保留原日志并检查写入或手工编辑问题",
            )
        expected_sequence += 1
        run_id = record.get("run_id")
        if (
            not isinstance(run_id, str)
            or (run_match := _RUN_DIRECTORY.fullmatch(run_id)) is None
            or not 0 < int(run_match["number"]) <= _MAX_U64
        ):
            fail(str(source), f"第 {line_number} 行缺少有效 run_id", "使用 ATT 完整写入的项目日志")
        if expected_run_id is None:
            expected_run_id = run_id
        elif run_id != expected_run_id:
            fail(str(source), f"第 {line_number} 行 run_id 与本文件前文不一致", "每份日志只保留同一次运行")
        for field in ("timestamp", "level", "event"):
            value = record.get(field)
            if not isinstance(value, str) or not value.strip():
                fail(str(source), f"第 {line_number} 行缺少有效 {field}", "使用 ATT 完整写入的项目日志")
        _safe_public_text(record.get("message"), f"{source}:第 {line_number} 行 message")
        event = cast(str, record["event"])
        level = cast(str, record["level"])
        if event not in _EVENTS:
            fail(str(source), f"第 {line_number} 行 event 不是当前项目日志事件", "恢复 ATT 当前版本日志")
        if level not in {"error", "warn", "info", "debug"}:
            fail(str(source), f"第 {line_number} 行 level 不是当前项目日志级别", "恢复 ATT 当前版本日志")
        context = _project_log_context(record.get("context"), f"{source}:第 {line_number} 行 context")
        if expected_context is None:
            expected_context = context
        elif context != expected_context:
            fail(str(source), f"第 {line_number} 行 context 与本文件前文不一致", "每份日志只保留同一次运行")
        if not isinstance(record.get("payload"), dict):
            fail(str(source), f"第 {line_number} 行缺少有效 payload", "使用 ATT 项目日志")
        _timestamp(record["timestamp"], str(source), line_number)
        result.append(record)
    if not result:
        fail(str(source), "项目日志为空", "提供 ATT 完整写入的非空项目日志")
    started = [index for index, record in enumerate(result) if record.get("event") == "run.started"]
    if started != [0]:
        fail(str(source), "run.started 不是唯一且第一条记录", "保留 ATT 从启动开始的完整项目日志")
    finished = [index for index, record in enumerate(result) if record.get("event") == "run.finished"]
    if finished != [len(result) - 1]:
        fail(str(source), "run.finished 不是唯一且最后一条记录", "保留 ATT 完整关闭后的项目日志")
    return result


def _timestamp(value: JsonValue, object_name: str, line_number: int) -> datetime:
    if not isinstance(value, str):
        fail(object_name, f"第 {line_number} 行 timestamp 不是 string", "使用 ATT 项目日志的 RFC 3339 时间")
    normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        fail(object_name, f"第 {line_number} 行 timestamp 不是有效 ISO 8601 时间", "恢复 ATT 原始项目日志")
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        fail(object_name, f"第 {line_number} 行 timestamp 缺少时区", "使用带 Z 或明确 UTC offset 的时间")
    return parsed


def _exact_object(
    value: JsonValue,
    object_name: str,
    fields: set[str],
) -> dict[str, JsonValue]:
    if not isinstance(value, dict) or set(value) != fields:
        fail(object_name, f"字段必须恰好为 {', '.join(sorted(fields))}", "恢复 ATT 当前版本的项目日志")
    return value


def _u64(value: JsonValue, object_name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= _MAX_U64:
        fail(object_name, "值不是 u64 非负整数", "恢复 ATT 原始项目日志")
    return value


def _phase_name(value: JsonValue, object_name: str) -> str:
    if not isinstance(value, str) or value not in _PHASES:
        fail(object_name, "phase 不是当前项目日志阶段枚举", "恢复 ATT 当前版本的项目日志")
    return value


def _phase_amount(value: JsonValue, object_name: str) -> None:
    if not isinstance(value, dict):
        fail(object_name, "amount 不是 object", "恢复 ATT 原始项目日志")
    kind = value.get("kind")
    if kind == "indeterminate":
        _exact_object(value, object_name, {"kind"})
        return
    if kind == "determinate":
        amount = _exact_object(value, object_name, {"kind", "completed", "total"})
        _u64(amount["completed"], f"{object_name}.completed")
        _u64(amount["total"], f"{object_name}.total")
        return
    fail(object_name, "amount.kind 不是 indeterminate 或 determinate", "恢复 ATT 当前版本的项目日志")


def _phase_payload(event: str, payload: dict[str, JsonValue], object_name: str) -> tuple[str, JsonValue]:
    if event in {"phase.started", "phase.completed"}:
        wire = _exact_object(payload, object_name, {"phase", "amount"})
        phase = _phase_name(wire["phase"], object_name)
        _phase_amount(wire["amount"], f"{object_name}.amount")
        return phase, None
    wire = _exact_object(payload, object_name, {"phase", "outcome"})
    phase = _phase_name(wire["phase"], object_name)
    outcome = _exact_object(wire["outcome"], f"{object_name}.outcome", {"kind"})
    if not isinstance(outcome["kind"], str) or outcome["kind"] not in {"failed", "cancelled"}:
        fail(
            f"{object_name}.outcome",
            "kind 不是 failed 或 cancelled",
            "恢复 ATT 当前版本的项目日志",
        )
    return phase, outcome


def _task_outcome(value: JsonValue, object_name: str) -> str:
    outcome = _exact_object(value, object_name, {"kind"})
    kind = outcome["kind"]
    if not isinstance(kind, str) or kind not in _TASK_OUTCOMES:
        fail(object_name, "kind 不是当前 task.finished 结果枚举", "恢复 ATT 当前版本的项目日志")
    return kind


def _task_counters(value: JsonValue, object_name: str) -> dict[str, int]:
    counters = _exact_object(value, object_name, _TASK_COUNTER_FIELDS)
    result = {field: _u64(counters[field], f"{object_name}.{field}") for field in _TASK_COUNTER_FIELDS}
    terminal = sum(result[field] for field in ("complete", "partial", "unavailable", "failed", "cancelled"))
    if result["started"] != terminal or result["planned"] != result["started"] + result["not_started"]:
        fail(object_name, "任务计数不满足 started/planned 汇总关系", "恢复 ATT 原始项目日志")
    return result


def _translation_summary(value: JsonValue, object_name: str) -> dict[str, int]:
    wire = _exact_object(value, object_name, {"engine", "summary"})
    engine = wire["engine"]
    if engine == "generic":
        fields = _GENERIC_SUMMARY_FIELDS
    elif engine == "rpg_maker":
        fields = _RPG_MAKER_SUMMARY_FIELDS
    else:
        fail(object_name, "engine 不是 generic 或 rpg_maker", "恢复 ATT 当前版本的项目日志")
    summary = _exact_object(wire["summary"], f"{object_name}.summary", fields)
    numeric_fields = fields - {"request_admission_stopped"}
    numeric = {field: _u64(summary[field], f"{object_name}.summary.{field}") for field in numeric_fields}
    if not isinstance(summary["request_admission_stopped"], bool):
        fail(
            f"{object_name}.summary.request_admission_stopped",
            "值不是 boolean",
            "恢复 ATT 原始项目日志",
        )
    if engine == "generic":
        planned = numeric["planned_units"]
        remaining = numeric["remaining_units"]
        if remaining > planned:
            fail(object_name, "remaining_units 大于 planned_units", "恢复 ATT 原始项目日志")
        resolved = planned - remaining
        if (
            resolved > numeric["accepted_units"] + numeric["reused_units"]
            or numeric["accepted_units"] > planned
            or resolved > numeric["written_units"]
            or numeric["written_units"] > resolved + numeric["reused_units"]
            or numeric["rejected_units"] > remaining
        ):
            fail(object_name, "Generic summary 计数关系不一致", "恢复 ATT 原始项目日志")
    elif (
        numeric["accepted_decisions"] > numeric["written_locations"]
        or numeric["remaining_decisions"] > numeric["remaining_locations"]
        or numeric["rejected_locations"] > numeric["remaining_locations"]
    ):
        fail(object_name, "RPG Maker summary 计数关系不一致", "恢复 ATT 原始项目日志")
    return numeric


def _translation_result(value: JsonValue, object_name: str) -> dict[str, JsonValue]:
    if not isinstance(value, dict):
        fail(object_name, "result 不是 object", "恢复 ATT 原始项目日志")
    kind = value.get("kind")
    if kind == "not_started":
        return _exact_object(value, object_name, {"kind"})
    if not isinstance(kind, str) or kind not in {
        "no_work",
        "complete",
        "incomplete",
        "failed",
        "cancelled",
    }:
        fail(object_name, "kind 不是当前 translation.finished 结果枚举", "恢复 ATT 当前版本的项目日志")
    result = _exact_object(value, object_name, {"kind", "tasks", "summary"})
    counters = _task_counters(result["tasks"], f"{object_name}.tasks")
    if kind == "no_work" and counters["planned"] != 0:
        fail(object_name, "no_work 的 planned 不是 0", "恢复 ATT 原始项目日志")
    summary = result["summary"]
    if kind in {"failed", "cancelled"} and summary is None:
        return result
    numeric_summary = _translation_summary(summary, f"{object_name}.summary")
    if kind in {"no_work", "complete"}:
        remaining = (
            numeric_summary["remaining_units"] + numeric_summary["rejected_units"]
            if "remaining_units" in numeric_summary
            else numeric_summary["remaining_decisions"]
            + numeric_summary["remaining_locations"]
            + numeric_summary["rejected_locations"]
        )
        if remaining != 0:
            fail(object_name, f"{kind} 仍包含未完成工作", "恢复 ATT 原始项目日志")
    return result


def _run_result(value: JsonValue, object_name: str) -> dict[str, JsonValue]:
    result = _exact_object(value, object_name, {"kind"})
    if not isinstance(result["kind"], str) or result["kind"] not in {
        "succeeded",
        "cancelled",
        "failed",
        "recovery_required",
        "outcome_unknown",
    }:
        fail(object_name, "kind 不是当前 run.finished 结果枚举", "恢复 ATT 当前版本的项目日志")
    return result


def _require_level(level: JsonValue, expected: str, object_name: str) -> None:
    if level != expected:
        fail(object_name, f"level 必须是 {expected}", "恢复 ATT 当前版本的项目日志")


def _text(value: JsonValue, object_name: str, *, allow_empty: bool = False) -> str:
    return _safe_public_text(value, object_name, require_non_blank=not allow_empty)


def _task_position(value: JsonValue, object_name: str) -> tuple[int, int]:
    task = _exact_object(value, object_name, {"ordinal", "total"})
    ordinal = _u64(task["ordinal"], f"{object_name}.ordinal")
    total = _u64(task["total"], f"{object_name}.total")
    if ordinal == 0 or ordinal > total:
        fail(object_name, "任务序号不在 1..=total", "恢复 ATT 原始项目日志")
    return ordinal, total


def _run_plan(value: JsonValue, object_name: str) -> str:
    if not isinstance(value, dict):
        fail(object_name, "plan 不是 object", "恢复 ATT 原始项目日志")
    kind = value.get("kind")
    if kind == "init":
        plan = _exact_object(value, object_name, {"kind", "source", "game_root"})
        _text(plan["game_root"], f"{object_name}.game_root")
    elif kind == "extract":
        plan = _exact_object(value, object_name, {"kind", "source", "selection"})
        selection = plan["selection"]
        if not isinstance(selection, dict):
            fail(f"{object_name}.selection", "selection 不是 object", "恢复 ATT 原始项目日志")
        engine = selection.get("engine")
        if engine == "generic_jsonl":
            _exact_object(selection, f"{object_name}.selection", {"engine"})
        elif engine == "rpg_maker":
            selected = _exact_object(selection, f"{object_name}.selection", {"engine", "owners"})
            owners = _exact_object(
                selected["owners"], f"{object_name}.selection.owners", {"builtin", "rules"}
            )
            if not isinstance(owners["builtin"], bool) or not isinstance(owners["rules"], bool):
                fail(f"{object_name}.selection.owners", "owner 选择不是 boolean", "恢复 ATT 原始项目日志")
        else:
            fail(f"{object_name}.selection", "engine 不是当前 Extract 选择枚举", "恢复 ATT 当前版本日志")
    elif kind == "translate":
        plan = _exact_object(
            value,
            object_name,
            {"kind", "source", "profile", "terminology", "placeholders"},
        )
        _text(plan["profile"], f"{object_name}.profile")
        for field in ("terminology", "placeholders"):
            if plan[field] is not None:
                _text(plan[field], f"{object_name}.{field}")
    else:
        fail(object_name, "kind 不是 init、extract 或 translate", "恢复 ATT 当前版本的项目日志")
    if not isinstance(plan["source"], str) or plan["source"] not in {
        "explicit",
        "project_state",
        "product_default",
    }:
        fail(f"{object_name}.source", "来源不是当前 RunPlan 枚举", "恢复 ATT 当前版本的项目日志")
    return cast(str, kind)


def _run_plan_finalization(value: JsonValue, object_name: str) -> str:
    result = _exact_object(value, object_name, {"kind", "transaction", "run_continues"})
    kind = result["kind"]
    transaction = result["transaction"]
    if not isinstance(result["run_continues"], bool):
        fail(object_name, "run_continues 不是 boolean", "恢复 ATT 原始项目日志")
    allowed = {
        "saved": {"committed"},
        "not_saved": {"not_started", "rolled_back"},
        "saved_finalization_failed": {"committed"},
        "outcome_unknown": {"outcome_unknown"},
    }
    if (
        not isinstance(kind, str)
        or not isinstance(transaction, str)
        or kind not in allowed
        or transaction not in allowed[kind]
    ):
        fail(object_name, "kind 与 transaction 不符合当前 RunPlan 终态", "恢复 ATT 当前版本日志")
    return kind


def _publication_summary(value: JsonValue, object_name: str) -> None:
    wire = _exact_object(value, object_name, {"engine", "summary"})
    engine = wire["engine"]
    if engine == "generic":
        fields = _GENERIC_PUBLICATION_FIELDS
    elif engine == "rpg_maker":
        fields = _RPG_MAKER_PUBLICATION_FIELDS
    else:
        fail(object_name, "engine 不是 generic 或 rpg_maker", "恢复 ATT 当前版本的项目日志")
    summary = _exact_object(wire["summary"], f"{object_name}.summary", fields)
    for field in fields:
        _u64(summary[field], f"{object_name}.summary.{field}")


def _publication_result(value: JsonValue, object_name: str) -> str:
    if not isinstance(value, dict):
        fail(object_name, "result 不是 object", "恢复 ATT 原始项目日志")
    kind = value.get("kind")
    if kind == "published":
        result = _exact_object(value, object_name, {"kind", "summary"})
        _publication_summary(result["summary"], f"{object_name}.summary")
        return "published"
    if isinstance(kind, str) and kind in {"not_published", "recovery_required", "outcome_unknown"}:
        _exact_object(value, object_name, {"kind"})
        return kind
    fail(object_name, "kind 不是当前 publication.finished 结果枚举", "恢复 ATT 当前版本日志")


def _performance_snapshot(value: JsonValue, object_name: str) -> None:
    snapshot = _exact_object(value, object_name, {"sqlite_transactions", "candidate_validations"})
    transactions = _exact_object(
        snapshot["sqlite_transactions"],
        f"{object_name}.sqlite_transactions",
        {"read_snapshot", "write_plan", "database_initialization", "interactive"},
    )
    for scope_name, scope_value in transactions.items():
        scope = _exact_object(
            scope_value,
            f"{object_name}.sqlite_transactions.{scope_name}",
            {"begin", "commit", "rollback"},
        )
        for control_name, control_value in scope.items():
            control = _exact_object(
                control_value,
                f"{object_name}.sqlite_transactions.{scope_name}.{control_name}",
                {"attempted", "succeeded"},
            )
            attempted = _u64(control["attempted"], f"{object_name}.{scope_name}.{control_name}.attempted")
            succeeded = _u64(control["succeeded"], f"{object_name}.{scope_name}.{control_name}.succeeded")
            if succeeded > attempted:
                fail(object_name, "SQLite succeeded 大于 attempted", "恢复 ATT 原始项目日志")
    validation = _exact_object(
        snapshot["candidate_validations"], f"{object_name}.candidate_validations", {"started", "completed"}
    )
    started = _u64(validation["started"], f"{object_name}.candidate_validations.started")
    completed = _u64(validation["completed"], f"{object_name}.candidate_validations.completed")
    if completed > started:
        fail(object_name, "candidate validation completed 大于 started", "恢复 ATT 原始项目日志")


def _summarize_one(path: Path) -> dict[str, JsonValue]:
    records = _read_records(path)
    phases: list[JsonValue] = []
    seen_phases: set[str] = set()
    active_phase_starts: dict[str, tuple[datetime, int]] = {}
    terminal_phases: set[str] = set()
    task_outcomes: Counter[str] = Counter()
    diagnostics: list[JsonValue] = []
    translation: JsonValue = None
    final: JsonValue = None
    context = cast(dict[str, JsonValue], records[0]["context"])
    expected_summary_engine = "generic" if context["engine"] == "generic" else "rpg_maker"
    run_id: JsonValue = records[0].get("run_id") if records else None
    translation_finished_count = 0
    run_finished_count = 0
    task_total: int | None = None
    started_tasks: set[int] = set()
    finished_tasks: dict[int, str] = {}
    finished_attempts: dict[int, int] = {}
    retry_attempted_values: list[int] = []
    terminal_task_counters: dict[str, int] | None = None
    cancellation_requested = False
    run_plan_kind: str | None = None
    run_plan_finalized = False
    publication_state: str | None = None

    def validate_task_counters(counters: Mapping[str, int], object_name: str) -> None:
        expected_outcomes = Counter(
            "failed" if kind == "not_committed_after_earlier_failure" else kind
            for kind in finished_tasks.values()
        )
        if (
            counters["started"] != len(finished_tasks)
            or any(
                counters[field] != expected_outcomes[field]
                for field in ("complete", "partial", "unavailable", "failed", "cancelled")
            )
            or (task_total is not None and counters["planned"] != task_total)
            or not started_tasks.issubset(finished_tasks)
        ):
            fail(
                object_name,
                "translation.finished 任务计数与实际生命周期不一致",
                "恢复 ATT 原始项目日志",
            )

    for record in records:
        event = cast(str, record["event"])
        payload = cast(dict[str, JsonValue], record["payload"])
        level = cast(str, record["level"])
        sequence = cast(int, record["sequence"])
        event_object = f"{path}:第 {sequence} 条 {event}"
        if translation_finished_count > 0 and event in {"task.started", "task.finished"}:
            fail(event_object, "翻译终态之后又出现模型任务事件", "恢复 ATT 原始项目日志")
        if event == "run.started":
            _exact_object(payload, event_object, set())
            _require_level(level, "info", event_object)
        elif event == "run.cancel_requested":
            wire = _exact_object(payload, event_object, {"confirmed", "total"})
            confirmed = _u64(wire["confirmed"], f"{event_object}.confirmed")
            total_value = wire["total"]
            if total_value is not None:
                total = _u64(total_value, f"{event_object}.total")
                if confirmed > total:
                    fail(event_object, "confirmed 大于 total", "恢复 ATT 原始项目日志")
            if cancellation_requested:
                fail(event_object, "取消请求重复出现", "恢复 ATT 原始项目日志")
            cancellation_requested = True
            _require_level(level, "warn", event_object)
        elif event == "phase.started":
            phase, _ = _phase_payload(event, payload, event_object)
            _require_level(level, "info", event_object)
            if phase in seen_phases:
                fail(event_object, f"阶段 {phase} 重复开始或已经结束", "恢复 ATT 原始项目日志")
            seen_phases.add(phase)
            active_phase_starts[phase] = (
                _timestamp(record.get("timestamp"), str(path), sequence),
                sequence,
            )
        elif event in {"phase.completed", "phase.stopped"}:
            phase, stop_outcome = _phase_payload(event, payload, event_object)
            if event == "phase.completed":
                _require_level(level, "info", event_object)
            else:
                outcome_kind = cast(dict[str, JsonValue], stop_outcome)["kind"]
                _require_level(level, "error" if outcome_kind == "failed" else "warn", event_object)
            if phase in terminal_phases:
                fail(event_object, f"阶段 {phase} 出现多个结束事件", "恢复 ATT 原始项目日志")
            terminal_phases.add(phase)
            if phase not in seen_phases:
                seen_phases.add(phase)
                continue
            start = active_phase_starts.pop(phase)
            end_time = _timestamp(record.get("timestamp"), str(path), sequence)
            duration = (end_time - start[0]).total_seconds()
            if duration < 0:
                fail(str(path), f"阶段 {phase} 的结束时间早于开始时间", "恢复未修改的 ATT 项目日志")
            phases.append(
                {
                    "phase": phase,
                    "result": "completed" if event == "phase.completed" else "stopped",
                    "start_sequence": start[1],
                    "end_sequence": record.get("sequence"),
                    "duration_seconds": duration,
                    "outcome": stop_outcome,
                }
            )
        elif event == "run_plan.resolved":
            wire = _exact_object(payload, event_object, {"plan"})
            plan_kind = _run_plan(wire["plan"], f"{event_object}.plan")
            if run_plan_kind is not None:
                fail(event_object, "run_plan.resolved 重复出现", "恢复 ATT 原始项目日志")
            if plan_kind != context["command"]:
                fail(event_object, "RunPlan 类型与 context.command 不一致", "恢复 ATT 原始项目日志")
            run_plan_kind = plan_kind
            _require_level(level, "info", event_object)
        elif event == "run_plan.finalized":
            wire = _exact_object(payload, event_object, {"database", "result"})
            _text(wire["database"], f"{event_object}.database")
            result_kind = _run_plan_finalization(wire["result"], f"{event_object}.result")
            if run_plan_kind is None or run_plan_finalized:
                fail(event_object, "run_plan.finalized 没有唯一的 resolved 前序", "恢复 ATT 原始项目日志")
            run_plan_finalized = True
            _require_level(level, "info" if result_kind == "saved" else "error", event_object)
        elif event == "task.started":
            wire = _exact_object(payload, event_object, {"task"})
            ordinal, total = _task_position(wire["task"], f"{event_object}.task")
            if (
                (task_total is not None and total != task_total)
                or ordinal in started_tasks
                or ordinal in finished_tasks
            ):
                fail(event_object, "任务 total 不一致或同一任务重复开始", "恢复 ATT 原始项目日志")
            task_total = total
            started_tasks.add(ordinal)
            _require_level(level, "info", event_object)
        elif event == "task.finished":
            wire = _exact_object(payload, event_object, {"task", "attempts", "provider", "outcome"})
            ordinal, total = _task_position(wire["task"], f"{event_object}.task")
            attempts = _u64(wire["attempts"], f"{event_object}.attempts")
            if attempts == 0:
                fail(event_object, "已结束任务的 attempts 必须大于 0", "恢复 ATT 原始项目日志")
            if (task_total is not None and task_total != total) or ordinal in finished_tasks:
                fail(event_object, "任务 total 不一致或同一任务重复结束", "恢复 ATT 原始项目日志")
            task_total = total
            provider = wire["provider"]
            if provider is not None:
                provider_text = _safe_public_text(provider, f"{event_object}.provider")
                if (
                    provider_text != provider_text.strip()
                    or len(provider_text.encode("utf-8")) > _MAX_PROVIDER_NAME_BYTES
                ):
                    fail(
                        f"{event_object}.provider",
                        f"provider 必须是去除首尾空白且不超过 {_MAX_PROVIDER_NAME_BYTES} UTF-8 字节的安全单行文本",
                        "恢复 ATT 原始项目日志",
                    )
            outcome_kind = _task_outcome(wire["outcome"], f"{event_object}.outcome")
            finished_tasks[ordinal] = outcome_kind
            finished_attempts[ordinal] = attempts
            expected_level = (
                "info"
                if outcome_kind == "complete"
                else "warn"
                if outcome_kind in {"partial", "unavailable", "cancelled"}
                else "error"
            )
            _require_level(level, expected_level, event_object)
            task_outcomes[outcome_kind] += 1
        elif event in _DIAGNOSTIC_EVENTS:
            expected = {"relation", "object", "reason", "impact", "help"}
            if set(payload) != expected:
                fail(str(path), f"{event} payload 不符合当前五字段格式", "恢复 ATT 当前版本的项目日志")
            relation = payload.get("relation")
            if not isinstance(relation, str) or relation not in {
                "primary",
                "cleanup",
                "rollback",
                "discard",
                "finalization",
                "shutdown",
                "observability",
            }:
                fail(str(path), f"{event} relation 无效", "使用项目日志规格列出的自然关系")
            diagnostic: dict[str, JsonValue] = {"event": event, "sequence": record.get("sequence")}
            for field in ("relation", "object", "reason", "impact", "help"):
                value = payload.get(field)
                diagnostic[field] = _safe_public_text(
                    value,
                    f"{event_object}.{field}",
                )
            diagnostics.append(diagnostic)
            _require_level(
                level,
                "error"
                if event in {"diagnostic.run", "diagnostic.run_plan", "diagnostic.publication"}
                else "warn",
                event_object,
            )
        elif event == "translation.finished":
            translation_finished_count += 1
            wire = _exact_object(payload, event_object, {"result"})
            translation = _translation_result(wire["result"], f"{event_object}.result")
            translation_kind = translation["kind"]
            summary_value = translation.get("summary")
            if isinstance(summary_value, dict) and summary_value.get("engine") != expected_summary_engine:
                fail(event_object, "translation summary 与 context.engine 不一致", "恢复 ATT 原始项目日志")
            tasks_value = translation.get("tasks")
            if tasks_value is not None:
                counters = _task_counters(tasks_value, f"{event_object}.result.tasks")
                terminal_task_counters = counters
                validate_task_counters(counters, event_object)
            elif started_tasks or finished_tasks:
                fail(event_object, "not_started 终态之前已经出现模型任务", "恢复 ATT 原始项目日志")
            expected_level = (
                "info"
                if translation_kind in {"no_work", "complete"}
                else "error"
                if translation_kind == "failed"
                else "warn"
            )
            _require_level(level, expected_level, event_object)
        elif event == "retry.summary":
            wire = _exact_object(payload, event_object, {"attempted", "recovered", "exhausted"})
            retry = {
                field: _u64(wire[field], f"{event_object}.{field}")
                for field in ("attempted", "recovered", "exhausted")
            }
            if retry["attempted"] != retry["recovered"] + retry["exhausted"]:
                fail(event_object, "retry.summary 计数关系不一致", "恢复 ATT 原始项目日志")
            retry_attempted_values.append(retry["attempted"])
            _require_level(level, "info", event_object)
        elif event == "publication.started":
            wire = _exact_object(payload, event_object, {"output_root"})
            _text(wire["output_root"], f"{event_object}.output_root")
            if publication_state is not None:
                fail(event_object, "publication.started 重复或已经结束", "恢复 ATT 原始项目日志")
            publication_state = "started"
            _require_level(level, "info", event_object)
        elif event == "publication.finished":
            wire = _exact_object(payload, event_object, {"result"})
            publication_kind = _publication_result(wire["result"], f"{event_object}.result")
            if publication_state != "started":
                fail(event_object, "publication.finished 缺少唯一的 started 前序", "恢复 ATT 原始项目日志")
            publication_state = "finished"
            publication_result = wire["result"]
            if publication_kind == "published" and (
                not isinstance(publication_result, dict)
                or not isinstance(publication_result.get("summary"), dict)
                or cast(dict[str, JsonValue], publication_result["summary"]).get("engine")
                != expected_summary_engine
            ):
                fail(event_object, "publication summary 与 context.engine 不一致", "恢复 ATT 原始项目日志")
            _require_level(level, "info" if publication_kind == "published" else "error", event_object)
        elif event == "lua.print":
            wire = _exact_object(payload, event_object, {"message"})
            _text(wire["message"], f"{event_object}.message", allow_empty=True)
            _require_level(level, "debug", event_object)
        elif event == "performance.counters":
            wire = _exact_object(payload, event_object, {"snapshot"})
            _performance_snapshot(wire["snapshot"], f"{event_object}.snapshot")
            _require_level(level, "info", event_object)
        elif event == "run.finished":
            run_finished_count += 1
            wire = _exact_object(payload, event_object, {"result"})
            final = _run_result(wire["result"], f"{event_object}.result")
            final_kind = final["kind"]
            _require_level(
                level,
                "info" if final_kind in {"succeeded", "cancelled"} else "error",
                event_object,
            )
    if translation_finished_count > 1:
        fail(str(path), "translation.finished 出现多次", "每次 Translate 运行只保留一条翻译终态")
    command = context.get("command")
    expected_translation_finished = 1 if command == "translate" else 0
    if translation_finished_count != expected_translation_finished:
        fail(
            str(path),
            "translation.finished 数量与 context.command 不一致",
            "恢复 ATT 当前命令完整关闭后的项目日志",
        )
    if run_finished_count != 1:
        fail(str(path), "run.finished 数量不是一条", "使用 ATT 完整关闭后的项目日志")
    if publication_state == "started":
        fail(str(path), "publication.started 缺少 finished 终态", "恢复 ATT 原始项目日志")
    final_kind = cast(dict[str, JsonValue], final)["kind"]
    if run_plan_kind is not None and not run_plan_finalized and final_kind == "succeeded":
        fail(str(path), "成功运行的 RunPlan 没有 finalized 终态", "恢复 ATT 原始项目日志")
    if terminal_task_counters is not None:
        validate_task_counters(terminal_task_counters, str(path))
    if not started_tasks.issubset(finished_tasks):
        fail(str(path), "存在已开始但未结束的模型任务", "恢复 ATT 完整关闭后的项目日志")
    extra_attempts = sum(attempts - 1 for attempts in finished_attempts.values())
    if any(attempted != extra_attempts for attempted in retry_attempted_values):
        fail(str(path), "retry.summary 与任务实际额外 attempt 数不一致", "恢复 ATT 原始项目日志")
    return {
        "log": str(path.resolve()),
        "run_id": run_id,
        "context": context,
        "records": len(records),
        "phases": phases,
        "task_outcomes": dict(sorted(task_outcomes.items())),
        "translation_finished": translation,
        "diagnostics": diagnostics,
        "run_finished": final,
        "planning_failed": any(
            isinstance(item, dict) and item.get("event") == "diagnostic.run_plan" for item in diagnostics
        ),
    }


def _task_records(path: Path | None) -> JsonValue:
    if path is None:
        return None
    root = require_directory(path, "ATT task-records 目录")
    inventory: list[tuple[int, int, str, str]] = []
    for record in safe_walk_files(root):
        run_match = _RUN_DIRECTORY.fullmatch(record.parent.name)
        task_match = _TASK_RECORD.fullmatch(record.name)
        if record.parent.parent != root or run_match is None or task_match is None:
            continue
        run_number = int(run_match["number"])
        task_number = int(task_match["number"])
        if not 0 < run_number <= _MAX_U64 or not 0 < task_number <= _MAX_U64:
            continue
        inventory.append((run_number, task_number, record.parent.name, record.name))
    runs: dict[str, list[JsonValue]] = {}
    for _run_number, _task_number, run_name, task_name in sorted(inventory):
        runs.setdefault(run_name, []).append(task_name)
    return {
        "root": str(root),
        "count": len(inventory),
        "runs": runs,
        "coverage_inference": "forbidden",
        "note": "这里列出符合当前自然文件名格式的任务记录；项目日志负责证明实际任务终态。",
    }


def _summarize(args: argparse.Namespace) -> int:
    inputs = [*args.log]
    if args.task_records is not None:
        inputs.append(args.task_records)
    protect_outputs([args.output], inputs=inputs, replace=args.replace)
    summaries = [_summarize_one(path) for path in args.log]
    result: dict[str, JsonValue] = {
        "runs": summaries,
        "task_records": _task_records(args.task_records),
        "notes": [
            "任务 planned/started/complete/partial/unavailable/failed/cancelled 以 translation.finished 为权威汇总。",
            "task-records 不参与覆盖率、恢复、重放或数据库状态推断。",
        ],
    }
    write_json(args.output, result, replace=args.replace)
    incomplete = 0
    failed_plans = 0
    for summary in summaries:
        if summary.get("planning_failed") is True:
            failed_plans += 1
        translation = summary.get("translation_finished")
        if isinstance(translation, dict) and translation.get("kind") in {
            "not_started",
            "incomplete",
            "failed",
            "cancelled",
        }:
            incomplete += 1
    print(
        f"已汇总 {len(summaries)} 次 ATT 运行；规划失败 {failed_plans} 次，"
        f"翻译未完整/失败/取消 {incomplete} 次。"
    )
    print(f"汇总结果：{display_path(args.output)}")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _summarize(parsed))
