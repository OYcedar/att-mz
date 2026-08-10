#!/usr/bin/env python3
"""从 ATT 公开项目 JSONL 与任务记录汇总实际运行结果。"""

from __future__ import annotations

import argparse
import sys
import unicodedata
from collections import Counter, defaultdict
from datetime import datetime
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    display_path,
    fail,
    parse_json_text,
    protect_outputs,
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
    "symbol_repair_attempted_units",
    "symbol_repair_repaired_units",
    "symbol_repair_skipped_units",
    "symbol_repair_replacements",
}
_RPG_MAKER_PUBLICATION_FIELDS = {
    "translated_units",
    "original_units",
    "auto_wrapped_units",
    "inserted_line_breaks",
    "inserted_fullwidth_indents",
    "manual_layout_units",
    "symbol_repair_attempted_units",
    "symbol_repair_repaired_units",
    "symbol_repair_skipped_units",
    "symbol_repair_replacements",
}


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="汇总阶段耗时、任务终态、规划诊断和 Translate 最终剩余量。")
    parser.add_argument("--log", type=Path, action="append", required=True, help="可重复传入 run-*.jsonl")
    parser.add_argument("--task-records", type=Path, help="可选 task-records 目录")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replace", action="store_true")
    return parser


def _read_records(path: Path) -> list[dict[str, JsonValue]]:
    source = require_file(path, "ATT 项目 JSONL")
    result: list[dict[str, JsonValue]] = []
    expected_sequence = 1
    expected_run_id: str | None = None
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
    for line_number, line in enumerate(source.read_text(encoding="utf-8-sig").splitlines(), start=1):
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
        if not isinstance(run_id, str) or not run_id.strip():
            fail(str(source), f"第 {line_number} 行缺少有效 run_id", "使用 ATT 完整写入的项目日志")
        if expected_run_id is None:
            expected_run_id = run_id
        elif run_id != expected_run_id:
            fail(str(source), f"第 {line_number} 行 run_id 与本文件前文不一致", "每份日志只保留同一次运行")
        for field in ("timestamp", "level", "event", "message"):
            value = record.get(field)
            if not isinstance(value, str) or not value.strip():
                fail(str(source), f"第 {line_number} 行缺少有效 {field}", "使用 ATT 完整写入的项目日志")
        event = cast(str, record["event"])
        level = cast(str, record["level"])
        if event not in _EVENTS:
            fail(str(source), f"第 {line_number} 行 event 不是当前项目日志事件", "恢复 ATT 当前版本日志")
        if level not in {"error", "warn", "info", "debug"}:
            fail(str(source), f"第 {line_number} 行 level 不是当前项目日志级别", "恢复 ATT 当前版本日志")
        if not isinstance(record.get("context"), dict) or not isinstance(record.get("payload"), dict):
            fail(str(source), f"第 {line_number} 行缺少有效 context/payload", "使用 ATT 项目日志")
        _timestamp(record["timestamp"], str(source), line_number)
        result.append(record)
    if not result:
        fail(str(source), "项目日志为空", "提供 ATT 完整写入的非空项目日志")
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


def _translation_summary(value: JsonValue, object_name: str) -> None:
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
    for field in numeric_fields:
        _u64(summary[field], f"{object_name}.summary.{field}")
    if not isinstance(summary["request_admission_stopped"], bool):
        fail(
            f"{object_name}.summary.request_admission_stopped",
            "值不是 boolean",
            "恢复 ATT 原始项目日志",
        )


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
    _translation_summary(summary, f"{object_name}.summary")
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
    if not isinstance(value, str) or (not allow_empty and not value.strip()):
        fail(object_name, "值不是有效 string", "恢复 ATT 原始项目日志")
    return value


def _task_position(value: JsonValue, object_name: str) -> None:
    task = _exact_object(value, object_name, {"ordinal", "total"})
    ordinal = _u64(task["ordinal"], f"{object_name}.ordinal")
    total = _u64(task["total"], f"{object_name}.total")
    if ordinal == 0 or ordinal > total:
        fail(object_name, "任务序号不在 1..=total", "恢复 ATT 原始项目日志")


def _run_plan(value: JsonValue, object_name: str) -> None:
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
    starts: dict[str, list[tuple[datetime, int]]] = defaultdict(list)
    task_outcomes: Counter[str] = Counter()
    diagnostics: list[JsonValue] = []
    translation: JsonValue = None
    final: JsonValue = None
    context: JsonValue = records[0].get("context") if records else None
    run_id: JsonValue = records[0].get("run_id") if records else None
    translation_finished_count = 0
    run_finished_count = 0
    for record in records:
        event = cast(str, record["event"])
        payload = cast(dict[str, JsonValue], record["payload"])
        level = cast(str, record["level"])
        sequence = cast(int, record["sequence"])
        event_object = f"{path}:第 {sequence} 条 {event}"
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
            _require_level(level, "warn", event_object)
        elif event == "phase.started":
            phase, _ = _phase_payload(event, payload, event_object)
            _require_level(level, "info", event_object)
            starts[phase].append(
                (
                    _timestamp(record.get("timestamp"), str(path), sequence),
                    sequence,
                )
            )
        elif event in {"phase.completed", "phase.stopped"}:
            phase, stop_outcome = _phase_payload(event, payload, event_object)
            if event == "phase.completed":
                _require_level(level, "info", event_object)
            else:
                outcome_kind = cast(dict[str, JsonValue], stop_outcome)["kind"]
                _require_level(level, "error" if outcome_kind == "failed" else "warn", event_object)
            if not starts[phase]:
                fail(str(path), f"阶段 {phase} 结束前没有 phase.started", "保留完整、自然顺序的项目日志")
            start = starts[phase].pop()
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
            _run_plan(wire["plan"], f"{event_object}.plan")
            _require_level(level, "info", event_object)
        elif event == "run_plan.finalized":
            wire = _exact_object(payload, event_object, {"database", "result"})
            _text(wire["database"], f"{event_object}.database")
            result_kind = _run_plan_finalization(wire["result"], f"{event_object}.result")
            _require_level(level, "info" if result_kind == "saved" else "error", event_object)
        elif event == "task.started":
            wire = _exact_object(payload, event_object, {"task"})
            _task_position(wire["task"], f"{event_object}.task")
            _require_level(level, "info", event_object)
        elif event == "task.finished":
            wire = _exact_object(payload, event_object, {"task", "attempts", "outcome"})
            _task_position(wire["task"], f"{event_object}.task")
            _u64(wire["attempts"], f"{event_object}.attempts")
            outcome_kind = _task_outcome(wire["outcome"], f"{event_object}.outcome")
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
                if (
                    not isinstance(value, str)
                    or not value.strip()
                    or any(
                        unicodedata.category(character) in {"Cc", "Cf", "Zl", "Zp"} or character == "\u0085"
                        for character in value
                    )
                ):
                    fail(str(path), f"{event} 的 {field} 不是非空安全单行文本", "恢复 ATT 原始项目日志")
                diagnostic[field] = value
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
            for field in ("attempted", "recovered", "exhausted"):
                _u64(wire[field], f"{event_object}.{field}")
            _require_level(level, "info", event_object)
        elif event == "publication.started":
            wire = _exact_object(payload, event_object, {"output_root"})
            _text(wire["output_root"], f"{event_object}.output_root")
            _require_level(level, "info", event_object)
        elif event == "publication.finished":
            wire = _exact_object(payload, event_object, {"result"})
            publication_kind = _publication_result(wire["result"], f"{event_object}.result")
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
    if run_finished_count != 1:
        fail(str(path), "run.finished 数量不是一条", "使用 ATT 完整关闭后的项目日志")
    dangling = [phase for phase, stack in starts.items() if stack]
    if dangling:
        fail(str(path), f"阶段没有完成或停止：{', '.join(sorted(dangling))}", "恢复 ATT 完整关闭后的项目日志")
    return {
        "log": str(path.resolve()),
        "run_id": run_id,
        "context": context,
        "records": len(records),
        "phases": phases,
        "open_phases": sorted(dangling),
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
    runs: dict[str, list[JsonValue]] = defaultdict(list)
    for record in sorted(safe_walk_files(root), key=lambda item: item.as_posix()):
        if (
            record.parent.parent != root
            or not record.parent.name.startswith("run-")
            or not record.name.startswith("task-")
            or record.suffix.lower() != ".md"
        ):
            continue
        runs[record.parent.name].append(record.name)
    return {
        "root": str(root),
        "count": sum(len(files) for files in runs.values()),
        "runs": {name: files for name, files in sorted(runs.items())},
        "coverage_inference": "forbidden",
        "note": "任务记录数量只证明实际发出过的模型任务，不是 Planner 覆盖率。",
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
        if isinstance(translation, dict) and translation.get("kind") in {"incomplete", "failed", "cancelled"}:
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
