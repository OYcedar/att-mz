#!/usr/bin/env python3
"""只读校验 ATT 翻译任务账本的结构与可追溯关系。"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections import defaultdict
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Final, cast, final

EXPECTED_H2: Final[tuple[str, ...]] = (
    "整体方案",
    "任务契约",
    "项目全局事实与影响分析",
    "阶段总览与当前恢复入口",
    "1. 解包（UNP）",
    "2. 提取（EXT）",
    "3. 翻译（TRN）",
    "4. 写回（WBK）",
    "5. 封包（RPK）",
    "证据登记",
    "方案与变更记录",
    "阻塞、剩余风险、恢复与移交",
    "最终完成判断",
)
PHASE_PREFIXES: Final[dict[str, str]] = {
    "1. 解包（UNP）": "UNP",
    "2. 提取（EXT）": "EXT",
    "3. 翻译（TRN）": "TRN",
    "4. 写回（WBK）": "WBK",
    "5. 封包（RPK）": "RPK",
}
PHASE_NAMES: Final[dict[str, str]] = {
    "解包": "UNP",
    "提取": "EXT",
    "翻译": "TRN",
    "写回": "WBK",
    "封包": "RPK",
}
RESPONSIBILITY_STATES: Final[frozenset[str]] = frozenset(
    {"TODO", "DOING", "BLOCKED", "DONE", "N/A", "SUPERSEDED", "CANCELLED"}
)
OPEN_STATES: Final[frozenset[str]] = frozenset({"TODO", "DOING", "BLOCKED"})
CHECKED_STATES: Final[frozenset[str]] = frozenset(
    {"DONE", "N/A", "SUPERSEDED", "CANCELLED"}
)
EXECUTION_RESULTS: Final[frozenset[str]] = frozenset(
    {
        "NotRun",
        "Succeeded",
        "Partial",
        "Unavailable",
        "Failed",
        "Cancelled",
        "OutcomeUnknown",
    }
)
TODO_REQUIRED_FIELDS: Final[tuple[str, ...]] = (
    "父项",
    "依赖",
    "完成条件",
    "执行者",
    "最近结果",
    "结果与证据",
    "剩余与恢复入口",
)
EVIDENCE_REQUIRED_FIELDS: Final[tuple[str, ...]] = (
    "类型与观察时间",
    "来源定位",
    "直接观察",
    "支持或否定",
    "适用范围",
    "当前有效性",
)
CHANGE_REQUIRED_FIELDS: Final[tuple[str, ...]] = (
    "时间",
    "配置变更",
    "触发证据",
    "原方案或判断",
    "新方案与理由",
    "替代方案、保留行为与代价",
    "受影响的 TODO、阶段与完成声明",
    "新增、替代或重新核实 TODO",
    "安全恢复入口",
)
CONFIG_CHANGE_FIELDS: Final[tuple[str, ...]] = (
    "根因",
    "现实消费者与影响范围",
    "修改 TODO",
    "修改前验证 TODO",
    "修改后验证 TODO",
)
TASK_CONTRACT_FIELDS: Final[tuple[str, ...]] = (
    "任务 ID",
    "总体状态",
    "创建时间",
    "最后更新时间",
    "当前唯一写入者",
    "任务根",
    "任务清单",
    "游戏根",
    "引擎与 ATT 项目",
    "ATT 项目工作区",
    "翻译资源根",
    "候选与最终输出",
    "用户目标",
    "成功条件",
    "必须保持的现有行为",
    "范围与停止线",
    "已授权副作用",
    "未授权或需用户选择",
)
FINAL_FIELDS: Final[tuple[str, ...]] = (
    "完成声明",
    "用户成功条件核对",
    "最终产物证据",
    "必须保持行为证据",
    "配置变更验收",
    "剩余风险",
)
TODO_RE: Final[re.Pattern[str]] = re.compile(
    r"^- \[([ xX])\] "
    + r"`((?:UNP|EXT|TRN|WBK|RPK)-(\d{3,}))` "
    + r"`\[(TODO|DOING|BLOCKED|DONE|N/A|SUPERSEDED|CANCELLED)\]` "
    + r"(.+?)\s*$"
)
FIELD_RE: Final[re.Pattern[str]] = re.compile(r"^\s*-\s+([^：]+)：(.*)$")
HEADING_RE: Final[re.Pattern[str]] = re.compile(r"^(#{1,6})\s+(.+?)\s*$")
TODO_ID_RE: Final[re.Pattern[str]] = re.compile(r"\b(?:UNP|EXT|TRN|WBK|RPK)-\d{3,}\b")
EVIDENCE_ID_RE: Final[re.Pattern[str]] = re.compile(r"\bEV-\d{3,}\b")
CHANGE_ID_RE: Final[re.Pattern[str]] = re.compile(r"\bCHG-\d{3,}\b")
FILENAME_RE: Final[re.Pattern[str]] = re.compile(
    r"^\d{8}-\d{6}-(?:mv|mz)-.+\.md$", re.IGNORECASE
)


@dataclass(frozen=True)
class Diagnostic:
    level: str
    code: str
    path: str
    line: int
    object_id: str | None
    message: str

    def as_json(self) -> dict[str, object]:
        return {
            "level": self.level,
            "code": self.code,
            "path": self.path,
            "line": self.line,
            "object_id": self.object_id,
            "message": self.message,
        }


@dataclass
class FieldValue:
    value: str
    line: int


@dataclass
class Todo:
    identifier: str
    prefix: str
    number: int
    checkbox: str
    state: str
    title: str
    line: int
    section: str
    fields: dict[str, FieldValue] = field(default_factory=dict)


@dataclass
class Record:
    identifier: str
    line: int
    fields: dict[str, FieldValue] = field(default_factory=dict)


@dataclass
class PhaseSummary:
    name: str
    state: str
    entry_gate: str
    exit_gate: str
    line: int


@dataclass(frozen=True)
class Arguments:
    ledger: Path
    final: bool
    output_format: str
    warnings_as_errors: bool


@final
class LedgerValidator:
    """解析并校验一个受限 Markdown 账本。"""

    def __init__(self, path: Path, text: str, final: bool) -> None:
        self.path = path
        self.path_text = str(path)
        self.lines = text.splitlines()
        self.visible_lines = mask_non_runtime_lines(self.lines)
        self.final = final
        self.diagnostics: list[Diagnostic] = []
        self.h2_lines: list[tuple[str, int]] = []
        self.section_by_line: dict[int, str] = {}
        self.section_fields: dict[str, dict[str, FieldValue]] = defaultdict(dict)
        self.todos: dict[str, Todo] = {}
        self.evidence: dict[str, Record] = {}
        self.changes: dict[str, Record] = {}
        self.phase_summaries: dict[str, PhaseSummary] = {}

    def error(
        self, code: str, line: int, message: str, object_id: str | None = None
    ) -> None:
        self.diagnostics.append(
            Diagnostic("ERROR", code, self.path_text, line, object_id, message)
        )

    def warning(
        self, code: str, line: int, message: str, object_id: str | None = None
    ) -> None:
        self.diagnostics.append(
            Diagnostic("WARNING", code, self.path_text, line, object_id, message)
        )

    def validate(self) -> list[Diagnostic]:
        self._parse_headings()
        self._parse_objects()
        self._validate_path_and_contract()
        self._validate_todos()
        self._validate_records()
        self._validate_phase_summaries()
        self._validate_relationships()
        self._validate_final_state()
        return sorted(
            self.diagnostics,
            key=lambda item: (
                item.line,
                0 if item.level == "ERROR" else 1,
                item.code,
                item.object_id or "",
            ),
        )

    def _parse_headings(self) -> None:
        h1: list[tuple[str, int]] = []
        current_h2 = ""
        for line_number, line in enumerate(self.visible_lines, start=1):
            match = HEADING_RE.match(line)
            if match is not None:
                level = len(match.group(1))
                title = match.group(2).strip()
                if level == 1:
                    h1.append((title, line_number))
                elif level == 2:
                    current_h2 = title
                    self.h2_lines.append((title, line_number))
            if current_h2:
                self.section_by_line[line_number] = current_h2

        if len(h1) != 1:
            self.error(
                "E_H1_COUNT",
                h1[0][1] if h1 else 1,
                f"必须恰有一个一级标题，实际为 {len(h1)} 个",
            )

        actual_h2 = tuple(title for title, _ in self.h2_lines)
        if actual_h2 != EXPECTED_H2:
            self.error(
                "E_SECTION_ORDER",
                self.h2_lines[0][1] if self.h2_lines else 1,
                "二级章节缺失、重复或顺序错误；必须严格采用模板的十三个二级章节",
            )

    def _parse_objects(self) -> None:
        active_todo: Todo | None = None
        active_record: Record | None = None

        for line_number, line in enumerate(self.visible_lines, start=1):
            section = self.section_by_line.get(line_number, "")
            heading = HEADING_RE.match(line)
            if heading is not None:
                active_todo = None
                active_record = None
                if len(heading.group(1)) == 3:
                    identifier = heading.group(2).strip()
                    if EVIDENCE_ID_RE.fullmatch(identifier):
                        active_record = self._new_record(
                            identifier, line_number, section, "证据登记", self.evidence
                        )
                    elif CHANGE_ID_RE.fullmatch(identifier):
                        active_record = self._new_record(
                            identifier,
                            line_number,
                            section,
                            "方案与变更记录",
                            self.changes,
                        )
                    elif identifier.startswith(("EV-", "CHG-")):
                        self.error(
                            "E_RECORD_ID_SYNTAX",
                            line_number,
                            "证据和变更标题必须使用 EV-001 或 CHG-001 形式",
                            identifier,
                        )
                continue

            todo_match = TODO_RE.match(line)
            if todo_match is not None:
                active_record = None
                identifier = todo_match.group(2)
                if identifier in self.todos:
                    self.error(
                        "E_DUPLICATE_TODO",
                        line_number,
                        f"TODO ID {identifier} 重复",
                        identifier,
                    )
                    active_todo = None
                    continue
                active_todo = Todo(
                    identifier=identifier,
                    prefix=identifier.split("-", 1)[0],
                    number=int(todo_match.group(3)),
                    checkbox=todo_match.group(1).lower(),
                    state=todo_match.group(4),
                    title=todo_match.group(5).strip(),
                    line=line_number,
                    section=section,
                )
                self.todos[identifier] = active_todo
                continue
            if re.match(r"^- \[[ xX]\]\s+", line):
                self.error(
                    "E_TODO_SYNTAX",
                    line_number,
                    "TODO 首行不符合模板规定的 checkbox、ID、状态和标题格式",
                )
                active_todo = None
                continue

            field_match = FIELD_RE.match(line)
            if field_match is None:
                continue
            label = field_match.group(1).strip()
            value = field_match.group(2).strip()
            if active_todo is not None and line.startswith("  "):
                self._add_field(
                    active_todo.fields,
                    label,
                    value,
                    line_number,
                    active_todo.identifier,
                )
            elif active_record is not None:
                self._add_field(
                    active_record.fields,
                    label,
                    value,
                    line_number,
                    active_record.identifier,
                )
            else:
                self._add_field(
                    self.section_fields[section], label, value, line_number, section
                )

        self._parse_phase_summary_table()

    def _new_record(
        self,
        identifier: str,
        line: int,
        section: str,
        expected_section: str,
        records: dict[str, Record],
    ) -> Record | None:
        if section != expected_section:
            self.error(
                "E_RECORD_SECTION",
                line,
                f"{identifier} 必须位于“{expected_section}”章节",
                identifier,
            )
        if identifier in records:
            self.error(
                "E_DUPLICATE_RECORD",
                line,
                f"记录 ID {identifier} 重复",
                identifier,
            )
            return None
        record = Record(identifier=identifier, line=line)
        records[identifier] = record
        return record

    def _add_field(
        self,
        fields: dict[str, FieldValue],
        label: str,
        value: str,
        line: int,
        object_id: str,
    ) -> None:
        if label in fields:
            self.error(
                "E_DUPLICATE_FIELD",
                line,
                f"字段“{label}”重复",
                object_id,
            )
            return
        fields[label] = FieldValue(value=value, line=line)

    def _parse_phase_summary_table(self) -> None:
        section = "阶段总览与当前恢复入口"
        for line_number, line in enumerate(self.visible_lines, start=1):
            if self.section_by_line.get(line_number) != section:
                continue
            if not line.lstrip().startswith("|"):
                continue
            cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
            if len(cells) != 6 or cells[0] not in PHASE_NAMES:
                continue
            name, state, entry_gate, exit_gate, _, _ = cells
            if name in self.phase_summaries:
                self.error(
                    "E_DUPLICATE_PHASE_SUMMARY",
                    line_number,
                    f"阶段总览中的“{name}”重复",
                    name,
                )
                continue
            self.phase_summaries[name] = PhaseSummary(
                name=name,
                state=state,
                entry_gate=entry_gate,
                exit_gate=exit_gate,
                line=line_number,
            )

    def _validate_path_and_contract(self) -> None:
        if not self.path.is_absolute():
            self.error("E_PATH_ABSOLUTE", 1, "账本路径必须是绝对路径")
        if not FILENAME_RE.fullmatch(self.path.name):
            self.error(
                "E_FILENAME",
                1,
                "文件名必须为 YYYYMMDD-HHmmss-<mv|mz>-<项目名>.md",
            )

        fields = self.section_fields.get("任务契约", {})
        self._require_fields(fields, TASK_CONTRACT_FIELDS, "任务契约", 1)
        task_id = fields.get("任务 ID")
        if task_id is not None and task_id.value != self.path.stem:
            self.error(
                "E_TASK_ID",
                task_id.line,
                "任务 ID 必须与文件 stem 完全一致",
                task_id.value,
            )
        writer = fields.get("当前唯一写入者")
        if writer is not None and writer.value != "主 Agent":
            self.error(
                "E_LEDGER_WRITER",
                writer.line,
                "当前唯一写入者必须为“主 Agent”",
            )
        overall = fields.get("总体状态")
        if overall is not None and overall.value not in RESPONSIBILITY_STATES:
            self.error(
                "E_OVERALL_STATE_VALUE",
                overall.line,
                "总体状态必须使用责任状态闭集",
            )

        task_root = fields.get("任务根")
        ledger_path = fields.get("任务清单")
        if task_root is not None and not is_blank(task_root.value):
            root = Path(task_root.value)
            if not root.is_absolute():
                self.error(
                    "E_TASK_ROOT_ABSOLUTE", task_root.line, "任务根必须是绝对路径"
                )
            elif normalized_path(self.path.parent) != normalized_path(
                root / "att-tasks"
            ):
                self.error(
                    "E_LEDGER_LOCATION",
                    task_root.line,
                    "账本必须直接位于 <任务根>/att-tasks/",
                )
        if ledger_path is not None and not is_blank(ledger_path.value):
            declared = Path(ledger_path.value)
            if not declared.is_absolute():
                self.error(
                    "E_LEDGER_PATH_ABSOLUTE",
                    ledger_path.line,
                    "任务清单字段必须是绝对路径",
                )
            elif normalized_path(declared) != normalized_path(self.path):
                self.error(
                    "E_LEDGER_PATH_MISMATCH",
                    ledger_path.line,
                    "任务清单字段与实际文件路径不一致",
                )

    def _validate_todos(self) -> None:
        for section, prefix in PHASE_PREFIXES.items():
            for suffix in ("001", "002", "003"):
                identifier = f"{prefix}-{suffix}"
                if identifier not in self.todos:
                    line = self._section_line(section)
                    self.error(
                        "E_FIXED_TODO_MISSING",
                        line,
                        f"阶段缺少固定责任 {identifier}",
                        identifier,
                    )

        for todo in self.todos.values():
            expected_prefix = PHASE_PREFIXES.get(todo.section)
            if expected_prefix is None:
                self.error(
                    "E_TODO_SECTION",
                    todo.line,
                    "TODO 必须位于五个阶段章节之一",
                    todo.identifier,
                )
            elif todo.prefix != expected_prefix:
                self.error(
                    "E_TODO_PREFIX",
                    todo.line,
                    f"{todo.identifier} 与所在阶段前缀 {expected_prefix} 不一致",
                    todo.identifier,
                )

            if todo.number < 1:
                self.error(
                    "E_TODO_NUMBER",
                    todo.line,
                    "TODO 编号必须从 001 开始",
                    todo.identifier,
                )
            self._require_fields(
                todo.fields,
                TODO_REQUIRED_FIELDS,
                todo.identifier,
                todo.line,
                allow_blank={"父项", "依赖", "结果与证据"},
            )

            checked = todo.checkbox == "x"
            if (todo.state in CHECKED_STATES) != checked:
                self.error(
                    "E_STATUS_CHECKBOX",
                    todo.line,
                    f"checkbox 与责任状态 {todo.state} 不一致",
                    todo.identifier,
                )

            result = todo.fields.get("最近结果")
            if result is not None and result.value not in EXECUTION_RESULTS:
                self.error(
                    "E_EXECUTION_RESULT",
                    result.line,
                    f"未知执行结果“{result.value}”",
                    todo.identifier,
                )
            evidence_field = todo.fields.get("结果与证据")
            evidence_refs: set[str] = (
                set(find_refs(EVIDENCE_ID_RE, evidence_field.value))
                if evidence_field is not None
                else set()
            )

            if todo.state == "DONE":
                if evidence_field is None or is_blank(evidence_field.value):
                    self.error(
                        "E_DONE_EVIDENCE",
                        todo.line,
                        "DONE 必须填写结果与证据",
                        todo.identifier,
                    )
                elif not evidence_refs:
                    self.error(
                        "E_DONE_EVIDENCE_REF",
                        evidence_field.line,
                        "DONE 必须引用至少一个 EV-*",
                        todo.identifier,
                    )
            elif todo.state == "N/A":
                _ = self._require_conditional_field(
                    todo, "适用性核实", require_evidence=True
                )
            elif todo.state == "BLOCKED":
                _ = self._require_conditional_field(todo, "阻塞原因")
                _ = self._require_conditional_field(todo, "已确认状态")
                recovery = todo.fields.get("剩余与恢复入口")
                if recovery is None or is_blank(recovery.value):
                    self.error(
                        "E_BLOCKED_RECOVERY",
                        todo.line,
                        "BLOCKED 必须填写安全恢复入口",
                        todo.identifier,
                    )
            elif todo.state == "SUPERSEDED":
                replacement = self._require_conditional_field(todo, "替代项")
                if replacement is not None and not TODO_ID_RE.search(replacement.value):
                    self.error(
                        "E_SUPERSEDED_TARGET",
                        replacement.line,
                        "SUPERSEDED 必须引用至少一个替代 TODO",
                        todo.identifier,
                    )
            elif todo.state == "CANCELLED":
                _ = self._require_conditional_field(
                    todo, "取消依据", require_evidence=True
                )

            if todo.number in (2, 3) and todo.state in {
                "N/A",
                "SUPERSEDED",
                "CANCELLED",
            }:
                self.error(
                    "E_GATE_TERMINAL_STATE",
                    todo.line,
                    "固定进入门和验收门只能保持开放、阻塞或以 DONE 收束",
                    todo.identifier,
                )

        for prefix in PHASE_PREFIXES.values():
            entry = self.todos.get(f"{prefix}-002")
            exit_gate = self.todos.get(f"{prefix}-003")
            if (
                entry is not None
                and exit_gate is not None
                and exit_gate.state == "DONE"
                and entry.state != "DONE"
            ):
                self.error(
                    "E_GATE_ORDER",
                    exit_gate.line,
                    f"{exit_gate.identifier} 完成前 {entry.identifier} 必须为 DONE",
                    exit_gate.identifier,
                )

    def _require_conditional_field(
        self, todo: Todo, label: str, require_evidence: bool = False
    ) -> FieldValue | None:
        field_value = todo.fields.get(label)
        if field_value is None or is_blank(field_value.value):
            self.error(
                "E_STATE_FIELD",
                todo.line,
                f"{todo.state} 必须填写“{label}”",
                todo.identifier,
            )
            return None
        if require_evidence and not EVIDENCE_ID_RE.search(field_value.value):
            self.error(
                "E_STATE_EVIDENCE",
                field_value.line,
                f"“{label}”必须引用 EV-*",
                todo.identifier,
            )
        return field_value

    def _validate_records(self) -> None:
        for record in self.evidence.values():
            self._require_fields(
                record.fields,
                EVIDENCE_REQUIRED_FIELDS,
                record.identifier,
                record.line,
            )
            validity = record.fields.get("当前有效性")
            if validity is not None:
                if validity.value == "有效":
                    pass
                elif validity.value.startswith("失效（") and validity.value.endswith(
                    "）"
                ):
                    change_refs = find_refs(CHANGE_ID_RE, validity.value)
                    if not change_refs:
                        self.error(
                            "E_EVIDENCE_VALIDITY",
                            validity.line,
                            "失效证据必须引用 CHG-*",
                            record.identifier,
                        )
                else:
                    self.error(
                        "E_EVIDENCE_VALIDITY",
                        validity.line,
                        "当前有效性只能为“有效”或“失效（CHG-*）”",
                        record.identifier,
                    )
        for record in self.changes.values():
            self._require_fields(
                record.fields, CHANGE_REQUIRED_FIELDS, record.identifier, record.line
            )
            config = record.fields.get("配置变更")
            if config is not None and config.value not in {"是", "否"}:
                self.error(
                    "E_CONFIG_CHANGE_VALUE",
                    config.line,
                    "配置变更只能填写“是”或“否”",
                    record.identifier,
                )
            if config is not None and config.value == "是":
                self._require_fields(
                    record.fields,
                    CONFIG_CHANGE_FIELDS,
                    record.identifier,
                    record.line,
                )
                self._validate_config_change_tasks(record)

        known_evidence = set(self.evidence)
        for todo in self.todos.values():
            for label in ("结果与证据", "适用性核实", "取消依据"):
                field_value = todo.fields.get(label)
                if field_value is not None:
                    self._validate_known_refs(
                        find_refs(EVIDENCE_ID_RE, field_value.value),
                        known_evidence,
                        "E_UNKNOWN_EVIDENCE",
                        field_value.line,
                        todo.identifier,
                    )

        for record in self.changes.values():
            trigger = record.fields.get("触发证据")
            if trigger is not None:
                refs = find_refs(EVIDENCE_ID_RE, trigger.value)
                if not refs:
                    self.error(
                        "E_CHANGE_TRIGGER",
                        trigger.line,
                        "变更记录必须引用至少一个 EV-*",
                        record.identifier,
                    )
                self._validate_known_refs(
                    refs,
                    known_evidence,
                    "E_UNKNOWN_EVIDENCE",
                    trigger.line,
                    record.identifier,
                )

        known_todos = set(self.todos)
        known_changes = set(self.changes)
        for record in self.evidence.values():
            supported = record.fields.get("支持或否定")
            if supported is not None:
                todo_refs = find_refs(TODO_ID_RE, supported.value)
                change_refs = find_refs(CHANGE_ID_RE, supported.value)
                if not todo_refs and not change_refs and "完成" not in supported.value:
                    self.error(
                        "E_EVIDENCE_TARGET",
                        supported.line,
                        "证据必须指向 TODO、CHG 或完成声明",
                        record.identifier,
                    )
                self._validate_known_refs(
                    todo_refs,
                    known_todos,
                    "E_UNKNOWN_TODO",
                    supported.line,
                    record.identifier,
                )
                self._validate_known_refs(
                    change_refs,
                    known_changes,
                    "E_UNKNOWN_CHANGE",
                    supported.line,
                    record.identifier,
                )
            validity = record.fields.get("当前有效性")
            if validity is not None:
                self._validate_known_refs(
                    find_refs(CHANGE_ID_RE, validity.value),
                    known_changes,
                    "E_UNKNOWN_CHANGE",
                    validity.line,
                    record.identifier,
                )

        for record in self.changes.values():
            for label in (
                "受影响的 TODO、阶段与完成声明",
                "新增、替代或重新核实 TODO",
                "修改 TODO",
                "修改前验证 TODO",
                "修改后验证 TODO",
            ):
                field_value = record.fields.get(label)
                if field_value is not None:
                    self._validate_known_refs(
                        find_refs(TODO_ID_RE, field_value.value),
                        known_todos,
                        "E_UNKNOWN_TODO",
                        field_value.line,
                        record.identifier,
                    )

    def _validate_config_change_tasks(self, record: Record) -> None:
        for label in ("修改 TODO", "修改前验证 TODO", "修改后验证 TODO"):
            field_value = record.fields.get(label)
            if field_value is None:
                continue
            refs = find_refs(TODO_ID_RE, field_value.value)
            if not refs:
                self.error(
                    "E_CONFIG_TASK_REF",
                    field_value.line,
                    f"“{label}”必须引用 TODO ID",
                    record.identifier,
                )
                continue
            for identifier in refs:
                todo = self.todos.get(identifier)
                if todo is None:
                    self.error(
                        "E_UNKNOWN_TODO",
                        field_value.line,
                        f"引用了不存在的 TODO {identifier}",
                        record.identifier,
                    )
                elif todo.state != "DONE":
                    if self.final:
                        self.error(
                            "E_OPEN_CONFIG_GATE",
                            field_value.line,
                            f"最终完成时配置变更责任 {identifier} 必须为 DONE",
                            record.identifier,
                        )
                    else:
                        self.warning(
                            "W_OPEN_CONFIG_GATE",
                            field_value.line,
                            f"配置变更责任 {identifier} 尚未 DONE",
                            record.identifier,
                        )

    def _validate_phase_summaries(self) -> None:
        if set(self.phase_summaries) != set(PHASE_NAMES):
            self.error(
                "E_PHASE_SUMMARY_SET",
                self._section_line("阶段总览与当前恢复入口"),
                "阶段总览必须恰好包含解包、提取、翻译、写回、封包五行",
            )
        for summary in self.phase_summaries.values():
            if summary.state not in RESPONSIBILITY_STATES:
                self.error(
                    "E_PHASE_STATE",
                    summary.line,
                    f"阶段状态“{summary.state}”不在责任状态闭集中",
                    summary.name,
                )
            prefix = PHASE_NAMES[summary.name]
            for label, identifier in (
                ("最新进入门", summary.entry_gate),
                ("最新验收门", summary.exit_gate),
            ):
                todo = self.todos.get(identifier)
                if todo is None:
                    self.error(
                        "E_PHASE_GATE_REF",
                        summary.line,
                        f"{label}引用了不存在的 TODO {identifier}",
                        summary.name,
                    )
                elif todo.prefix != prefix:
                    self.error(
                        "E_PHASE_GATE_PREFIX",
                        summary.line,
                        f"{label} {identifier} 不属于{summary.name}阶段",
                        summary.name,
                    )
                elif todo.state != "DONE":
                    if self.final:
                        self.error(
                            "E_OPEN_GATE",
                            summary.line,
                            f"最终完成时{label} {identifier} 必须为 DONE",
                            summary.name,
                        )
                    else:
                        self.warning(
                            "W_OPEN_GATE",
                            summary.line,
                            f"{label} {identifier} 尚未 DONE",
                            summary.name,
                        )

    def _validate_relationships(self) -> None:
        known_todos = set(self.todos)
        parent_graph: dict[str, set[str]] = defaultdict(set)
        dependency_graph: dict[str, set[str]] = defaultdict(set)
        replacement_graph: dict[str, set[str]] = defaultdict(set)

        for todo in self.todos.values():
            parent = todo.fields.get("父项")
            if parent is not None:
                refs = find_refs(TODO_ID_RE, parent.value)
                self._validate_known_refs(
                    refs,
                    known_todos,
                    "E_UNKNOWN_TODO",
                    parent.line,
                    todo.identifier,
                )
                for target in refs:
                    if target == todo.identifier:
                        self.error(
                            "E_SELF_REFERENCE",
                            parent.line,
                            "父项不能引用自身",
                            todo.identifier,
                        )
                    else:
                        parent_graph[todo.identifier].add(target)

            dependency = todo.fields.get("依赖")
            if dependency is not None:
                refs = find_refs(TODO_ID_RE, dependency.value)
                self._validate_known_refs(
                    refs,
                    known_todos,
                    "E_UNKNOWN_TODO",
                    dependency.line,
                    todo.identifier,
                )
                for target in refs:
                    if target == todo.identifier:
                        self.error(
                            "E_SELF_REFERENCE",
                            dependency.line,
                            "依赖不能引用自身",
                            todo.identifier,
                        )
                    else:
                        dependency_graph[todo.identifier].add(target)

            replacement = todo.fields.get("替代项")
            if replacement is not None:
                refs = find_refs(TODO_ID_RE, replacement.value)
                self._validate_known_refs(
                    refs,
                    known_todos,
                    "E_UNKNOWN_TODO",
                    replacement.line,
                    todo.identifier,
                )
                for target in refs:
                    if target == todo.identifier:
                        self.error(
                            "E_SELF_REFERENCE",
                            replacement.line,
                            "替代项不能引用自身",
                            todo.identifier,
                        )
                    else:
                        replacement_graph[todo.identifier].add(target)

        self._report_cycles(parent_graph, "E_PARENT_CYCLE", "父项")
        self._report_cycles(dependency_graph, "E_DEPENDENCY_CYCLE", "依赖")
        self._report_cycles(replacement_graph, "E_REPLACEMENT_CYCLE", "替代链")

        summary_fields = self.section_fields.get("阶段总览与当前恢复入口", {})
        current_entry = summary_fields.get("当前恢复入口")
        if current_entry is not None:
            refs = find_refs(TODO_ID_RE, current_entry.value)
            if len(refs) != 1:
                self.error(
                    "E_CURRENT_ENTRY",
                    current_entry.line,
                    "当前恢复入口必须引用恰好一个 TODO ID",
                )
            else:
                self._validate_known_refs(
                    refs,
                    known_todos,
                    "E_UNKNOWN_TODO",
                    current_entry.line,
                    "当前恢复入口",
                )

    def _report_cycles(self, graph: dict[str, set[str]], code: str, label: str) -> None:
        visiting: set[str] = set()
        visited: set[str] = set()

        def visit(node: str, trail: list[str]) -> None:
            if node in visiting:
                cycle_start = trail.index(node) if node in trail else 0
                cycle = trail[cycle_start:] + [node]
                todo = self.todos.get(node)
                self.error(
                    code,
                    todo.line if todo is not None else 1,
                    f"{label}存在环：{' -> '.join(cycle)}",
                    node,
                )
                return
            if node in visited:
                return
            visiting.add(node)
            trail.append(node)
            for target in graph.get(node, set()):
                visit(target, trail)
            _ = trail.pop()
            visiting.remove(node)
            visited.add(node)

        for node in graph:
            visit(node, [])

    def _validate_final_state(self) -> None:
        if not self.final:
            return

        final_fields = self.section_fields.get("最终完成判断", {})
        self._require_fields(
            final_fields,
            FINAL_FIELDS,
            "最终完成判断",
            self._section_line("最终完成判断"),
        )
        declaration = final_fields.get("完成声明")
        if declaration is not None and declaration.value != "完成":
            self.error(
                "E_FINAL_DECLARATION",
                declaration.line,
                "--final 要求“完成声明：完成”",
            )

        contract = self.section_fields.get("任务契约", {})
        overall = contract.get("总体状态")
        if overall is not None and overall.value != "DONE":
            self.error(
                "E_OVERALL_STATE",
                overall.line,
                "--final 要求“总体状态：DONE”",
            )

        for todo in self.todos.values():
            if todo.state in OPEN_STATES:
                self.error(
                    "E_OPEN_TODO",
                    todo.line,
                    f"最终完成时仍有开放责任 {todo.identifier} [{todo.state}]",
                    todo.identifier,
                )

        for summary in self.phase_summaries.values():
            if summary.state not in {"DONE", "N/A"}:
                self.error(
                    "E_OPEN_PHASE",
                    summary.line,
                    f"最终完成时阶段状态必须为 DONE 或 N/A，实际为 {summary.state}",
                    summary.name,
                )
            macro = self.todos.get(f"{PHASE_NAMES[summary.name]}-001")
            if macro is not None and macro.state not in {"DONE", "N/A"}:
                self.error(
                    "E_PHASE_MACRO",
                    macro.line,
                    "最终完成时阶段宏观责任必须为 DONE 或 N/A",
                    macro.identifier,
                )

        valid_evidence = {
            identifier
            for identifier, record in self.evidence.items()
            if (value := record.fields.get("当前有效性")) is not None
            and value.value == "有效"
        }
        for label in (
            "用户成功条件核对",
            "最终产物证据",
            "必须保持行为证据",
        ):
            value = final_fields.get(label)
            if value is None:
                continue
            refs = set(find_refs(EVIDENCE_ID_RE, value.value))
            if is_blank(value.value) or not refs:
                self.error(
                    "E_FINAL_EVIDENCE",
                    value.line,
                    f"“{label}”必须引用当前有效 EV-*",
                )
            elif not refs.issubset(valid_evidence):
                unknown_or_stale = ", ".join(sorted(refs - valid_evidence))
                self.error(
                    "E_FINAL_EVIDENCE_VALIDITY",
                    value.line,
                    f"“{label}”引用了不存在或已失效的证据：{unknown_or_stale}",
                )

        risks = final_fields.get("剩余风险")
        if risks is not None and is_blank(risks.value):
            self.error(
                "E_FINAL_RISKS",
                risks.line,
                "最终完成时必须明确填写剩余风险；没有则写“无”",
            )

    def _validate_known_refs(
        self,
        references: Iterable[str],
        known: set[str],
        code: str,
        line: int,
        object_id: str,
    ) -> None:
        for reference in references:
            if reference not in known:
                self.error(
                    code,
                    line,
                    f"引用了不存在的对象 {reference}",
                    object_id,
                )

    def _require_fields(
        self,
        fields: dict[str, FieldValue],
        required: Sequence[str],
        object_id: str,
        fallback_line: int,
        allow_blank: set[str] | None = None,
    ) -> None:
        blank_allowed = allow_blank or set()
        for label in required:
            value = fields.get(label)
            if value is None:
                self.error(
                    "E_REQUIRED_FIELD",
                    fallback_line,
                    f"缺少必填字段“{label}”",
                    object_id,
                )
            elif label not in blank_allowed and is_blank(value.value):
                self.error(
                    "E_EMPTY_FIELD",
                    value.line,
                    f"必填字段“{label}”不能为空或为占位符",
                    object_id,
                )

    def _section_line(self, title: str) -> int:
        for actual, line in self.h2_lines:
            if actual == title:
                return line
        return 1


def mask_non_runtime_lines(lines: Sequence[str]) -> list[str]:
    """屏蔽 fenced code 与 HTML 注释，同时保留原始行号。"""

    visible: list[str] = []
    fence_marker: str | None = None
    in_comment = False
    for line in lines:
        stripped = line.lstrip()
        if fence_marker is not None:
            visible.append("")
            if stripped.startswith(fence_marker):
                fence_marker = None
            continue
        if stripped.startswith("```"):
            fence_marker = "```"
            visible.append("")
            continue
        if stripped.startswith("~~~"):
            fence_marker = "~~~"
            visible.append("")
            continue

        output = line
        while True:
            if in_comment:
                end = output.find("-->")
                if end < 0:
                    output = ""
                    break
                output = output[end + 3 :]
                in_comment = False
                continue
            start = output.find("<!--")
            if start < 0:
                break
            end = output.find("-->", start + 4)
            if end < 0:
                output = output[:start]
                in_comment = True
                break
            output = output[:start] + output[end + 3 :]
        visible.append(output)
    return visible


def find_refs(pattern: re.Pattern[str], value: str) -> list[str]:
    return [match.group(0) for match in pattern.finditer(value)]


def is_blank(value: str) -> bool:
    stripped = value.strip()
    return stripped in {"", "—"} or (
        stripped.startswith("<") and stripped.endswith(">")
    )


def normalized_path(path: Path) -> str:
    return os.path.normcase(os.path.abspath(os.fspath(path)))


def parse_arguments(argv: Sequence[str]) -> Arguments:
    parser = argparse.ArgumentParser(
        description="只读校验 ATT 翻译任务账本的结构与可追溯关系。"
    )
    _ = parser.add_argument("ledger", type=Path, help="实体任务账本的绝对路径")
    _ = parser.add_argument("--final", action="store_true", help="额外校验最终完成声明")
    _ = parser.add_argument(
        "--format",
        choices=("text", "json"),
        default="text",
        dest="output_format",
        help="诊断输出格式",
    )
    _ = parser.add_argument(
        "--warnings-as-errors",
        action="store_true",
        help="存在警告且无错误时返回退出码 3",
    )
    namespace = parser.parse_args(argv)
    return Arguments(
        ledger=cast(Path, namespace.ledger),
        final=cast(bool, namespace.final),
        output_format=cast(str, namespace.output_format),
        warnings_as_errors=cast(bool, namespace.warnings_as_errors),
    )


def read_ledger(path: Path) -> str:
    if not path.is_absolute():
        raise ValueError("账本路径必须是绝对路径")
    return path.read_text(encoding="utf-8-sig", errors="strict")


def emit_diagnostics(diagnostics: Sequence[Diagnostic], output_format: str) -> None:
    if output_format == "json":
        print(
            json.dumps(
                [item.as_json() for item in diagnostics],
                ensure_ascii=False,
                indent=2,
            )
        )
        return
    for item in diagnostics:
        object_text = f" {item.object_id}" if item.object_id else ""
        message = (
            f"{item.level} {item.code} {item.path}:{item.line}"
            + f"{object_text}: {item.message}"
        )
        print(message)


def emit_invocation_error(path: Path, message: str, output_format: str) -> None:
    diagnostic = Diagnostic(
        level="ERROR",
        code="E_INPUT",
        path=str(path),
        line=0,
        object_id=None,
        message=message,
    )
    emit_diagnostics([diagnostic], output_format)


def main(argv: Sequence[str] | None = None) -> int:
    arguments = parse_arguments(sys.argv[1:] if argv is None else argv)
    try:
        text = read_ledger(arguments.ledger)
    except (OSError, UnicodeError, ValueError) as error:
        emit_invocation_error(arguments.ledger, str(error), arguments.output_format)
        return 2

    diagnostics = LedgerValidator(
        path=arguments.ledger,
        text=text,
        final=arguments.final,
    ).validate()
    emit_diagnostics(diagnostics, arguments.output_format)

    if any(item.level == "ERROR" for item in diagnostics):
        return 1
    if arguments.warnings_as_errors and any(
        item.level == "WARNING" for item in diagnostics
    ):
        return 3
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
