#!/usr/bin/env python3
"""只读检查 ATT 翻译任务清单的结构与引用关系。"""

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
    "任务信息",
    "项目概况与影响范围",
    "阶段总览与下一步",
    "1. 解包（UNP）",
    "2. 提取（EXT）",
    "3. 翻译（TRN）",
    "4. 写回（WBK）",
    "5. 封包（RPK）",
    "证据",
    "计划和变更记录",
    "阻塞、剩余工作和交接",
    "最终检查",
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
ADDITIONAL_FIXED_TODOS: Final[dict[str, str]] = {
    "TRN-004": "3. 翻译（TRN）",
}
FIXED_TODO_EXPECTATIONS: Final[dict[str, tuple[str, str | None, frozenset[str]]]] = {
    "UNP-001": ("阶段任务", None, frozenset({"UNP-002", "UNP-003"})),
    "UNP-002": ("阶段开始前检查", "UNP-001", frozenset()),
    "UNP-003": ("阶段完成检查", "UNP-001", frozenset({"UNP-002"})),
    "EXT-001": ("阶段任务", None, frozenset({"EXT-002", "EXT-003"})),
    "EXT-002": ("阶段开始前检查", "EXT-001", frozenset({"UNP-003"})),
    "EXT-003": ("阶段完成检查", "EXT-001", frozenset({"EXT-002"})),
    "TRN-001": ("阶段任务", None, frozenset({"TRN-002", "TRN-003"})),
    "TRN-002": ("阶段开始前检查", "TRN-001", frozenset({"EXT-003"})),
    "TRN-003": (
        "阶段完成检查",
        "TRN-001",
        frozenset({"TRN-002", "TRN-004"}),
    ),
    "TRN-004": ("术语检查", "TRN-001", frozenset({"TRN-002"})),
    "WBK-001": ("阶段任务", None, frozenset({"WBK-002", "WBK-003"})),
    "WBK-002": ("阶段开始前检查", "WBK-001", frozenset({"TRN-003"})),
    "WBK-003": ("阶段完成检查", "WBK-001", frozenset({"WBK-002"})),
    "RPK-001": ("阶段任务", None, frozenset({"RPK-002", "RPK-003"})),
    "RPK-002": ("阶段开始前检查", "RPK-001", frozenset({"WBK-003"})),
    "RPK-003": ("阶段完成检查", "RPK-001", frozenset({"RPK-002"})),
}
RESPONSIBILITY_STATES: Final[frozenset[str]] = frozenset(
    {"TODO", "DOING", "BLOCKED", "DONE", "N/A", "SUPERSEDED", "CANCELLED"}
)
OPEN_STATES: Final[frozenset[str]] = frozenset({"TODO", "DOING", "BLOCKED"})
CHECKED_STATES: Final[frozenset[str]] = frozenset(
    {"DONE", "N/A", "SUPERSEDED", "CANCELLED"}
)
TODO_TYPES: Final[frozenset[str]] = frozenset(
    {"阶段任务", "阶段开始前检查", "阶段完成检查", "术语检查", "执行", "调查"}
)
CHANGE_STATES: Final[frozenset[str]] = frozenset({"待处理", "已完成"})
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
    "任务类型",
    "上级 TODO",
    "需要先完成",
    "完成标准",
    "负责人",
    "最近执行结果",
    "结果和证据",
    "下一步",
)
EVIDENCE_REQUIRED_FIELDS: Final[tuple[str, ...]] = (
    "记录时间和类型",
    "来源",
    "观察结果",
    "支持或否定",
    "适用范围",
    "当前是否适用",
)
CHANGE_REQUIRED_FIELDS: Final[tuple[str, ...]] = (
    "时间",
    "处理状态",
    "是否修改项目配置",
    "触发证据",
    "原计划或判断",
    "新计划和原因",
    "其他方案及影响",
    "受影响范围",
    "重新检查 TODO",
    "下一步",
)
CONFIG_CHANGE_FIELDS: Final[tuple[str, ...]] = (
    "参考文档",
    "原因",
    "使用该配置的位置和影响范围",
    "修改前检查 TODO",
    "修改 TODO",
    "修改后检查 TODO",
)
TASK_CONTRACT_FIELDS: Final[tuple[str, ...]] = (
    "任务 ID",
    "总体状态",
    "创建时间",
    "最后更新时间",
    "当前唯一写入者",
    "任务目录",
    "任务清单",
    "游戏来源",
    "引擎与 ATT 项目",
    "ATT 项目目录",
    "翻译资源目录",
    "候选与最终文件",
    "实际使用的 ATT",
    "ATT 文档版本",
    "用户目标",
    "成功标准",
    "必须保持的行为",
    "任务范围",
    "已授权操作",
    "需要用户决定",
)
FINAL_FIELDS: Final[tuple[str, ...]] = (
    "完成状态",
    "用户目标",
    "最终文件",
    "必须保持的行为",
    "配置修改检查",
    "范围外风险",
)
TODO_RE: Final[re.Pattern[str]] = re.compile(
    r"^- \[([ xX])\] "
    + r"`((?:UNP|EXT|TRN|WBK|RPK)-(\d{3,}))` "
    + r"`\[(TODO|DOING|BLOCKED|DONE|N/A|SUPERSEDED|CANCELLED)\]` "
    + r"(.+?)\s*$"
)
FIELD_RE: Final[re.Pattern[str]] = re.compile(r"^\s*-\s+([^：]+)：(.*)$")
HEADING_RE: Final[re.Pattern[str]] = re.compile(r"^(#{1,6})\s+(.+?)\s*$")
TODO_ID_RE: Final[re.Pattern[str]] = re.compile(
    r"(?<![\w-])(?:UNP|EXT|TRN|WBK|RPK)-\d{3,}(?![\w-])"
)
EVIDENCE_ID_RE: Final[re.Pattern[str]] = re.compile(r"(?<![\w-])EV-\d{3,}(?![\w-])")
CHANGE_ID_RE: Final[re.Pattern[str]] = re.compile(r"(?<![\w-])CHG-\d{3,}(?![\w-])")
FINAL_ID_RE: Final[re.Pattern[str]] = re.compile(
    r"(?<![\w-])FINAL-(?:GOAL|OUTPUT|PRESERVE|CONFIG)(?![\w-])"
)
MARKDOWN_PATH_RE: Final[re.Pattern[str]] = re.compile(
    r"(?P<path>(?:[A-Za-z]:[\\/]|/)[^；，,\n]*?\.md)"
    r"(?P<anchor>#[^；，,\s)）]+)?"
    r"(?P<whole>\s*[（(]\s*全文\s*[）)])?",
    re.IGNORECASE,
)
FILENAME_RE: Final[re.Pattern[str]] = re.compile(
    r"^\d{8}-\d{6}-(?:mv|mz)-.+\.md$", re.IGNORECASE
)
PHASE_SUMMARY_HEADERS: Final[tuple[str, ...]] = (
    "阶段",
    "当前状态",
    "当前阶段任务",
    "当前开始前检查",
    "当前完成检查",
    "当前术语检查",
    "待重新验证的变更",
    "下一步",
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
    current_task: str
    start_check: str
    completion_check: str
    terminology_check: str
    pending_changes: str
    next_todo: str
    line: int


@dataclass(frozen=True)
class Arguments:
    task_list: Path
    final: bool
    output_format: str
    warnings_as_errors: bool


@final
class TaskListValidator:
    """解析并检查一个使用固定结构的 Markdown 任务清单。"""

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
        self.phase_summary_header_seen = False
        self.docs_root: Path | None = None
        self.change_affected_todos: dict[str, set[str]] = {}
        self.change_recheck_todos: dict[str, set[str]] = {}
        self.change_states: dict[str, str] = {}
        self.config_task_changes: dict[str, str] = {}

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
                            identifier, line_number, section, "证据", self.evidence
                        )
                    elif CHANGE_ID_RE.fullmatch(identifier):
                        active_record = self._new_record(
                            identifier,
                            line_number,
                            section,
                            "计划和变更记录",
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
                    "TODO 首行不符合模板规定的勾选框、ID、状态和标题格式",
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
        section = "阶段总览与下一步"
        for line_number, line in enumerate(self.visible_lines, start=1):
            if self.section_by_line.get(line_number) != section:
                continue
            if not line.lstrip().startswith("|"):
                continue
            cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
            if cells and cells[0] == "阶段":
                self.phase_summary_header_seen = True
                if tuple(cells) != PHASE_SUMMARY_HEADERS:
                    self.error(
                        "E_PHASE_SUMMARY_HEADER",
                        line_number,
                        "阶段总览表头必须严格匹配当前任务清单模板",
                    )
                continue
            if len(cells) != len(PHASE_SUMMARY_HEADERS) or cells[0] not in PHASE_NAMES:
                continue
            (
                name,
                state,
                current_task,
                start_check,
                completion_check,
                terminology_check,
                pending_changes,
                next_todo,
            ) = cells
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
                current_task=current_task,
                start_check=start_check,
                completion_check=completion_check,
                terminology_check=terminology_check,
                pending_changes=pending_changes,
                next_todo=next_todo,
                line=line_number,
            )

    def _validate_path_and_contract(self) -> None:
        if not self.path.is_absolute():
            self.error("E_PATH_ABSOLUTE", 1, "任务清单路径必须是绝对路径")
        if not FILENAME_RE.fullmatch(self.path.name):
            self.error(
                "E_FILENAME",
                1,
                "文件名必须为 YYYYMMDD-HHmmss-<mv|mz>-<项目名>.md",
            )

        fields = self.section_fields.get("任务信息", {})
        self._require_fields(fields, TASK_CONTRACT_FIELDS, "任务信息", 1)
        task_id = fields.get("任务 ID")
        if task_id is not None and task_id.value != self.path.stem:
            self.error(
                "E_TASK_ID",
                task_id.line,
                "任务 ID 必须与不含 .md 后缀的文件名完全一致",
                task_id.value,
            )
        writer = fields.get("当前唯一写入者")
        if writer is not None and writer.value != "主 Agent":
            self.error(
                "E_TASK_LIST_WRITER",
                writer.line,
                "当前唯一写入者必须为“主 Agent”",
            )
        overall = fields.get("总体状态")
        if overall is not None and overall.value not in RESPONSIBILITY_STATES:
            self.error(
                "E_OVERALL_STATE_VALUE",
                overall.line,
                "总体状态不是任务清单支持的状态",
            )

        task_root = fields.get("任务目录")
        task_list_path = fields.get("任务清单")
        if task_root is not None and not is_blank(task_root.value):
            root = Path(task_root.value)
            if not root.is_absolute():
                self.error(
                    "E_TASK_ROOT_ABSOLUTE", task_root.line, "任务目录必须是绝对路径"
                )
            elif normalized_path(self.path.parent) != normalized_path(
                root / "att-tasks"
            ):
                self.error(
                    "E_TASK_LIST_LOCATION",
                    task_root.line,
                    "任务清单必须直接位于 <任务目录>/att-tasks/",
                )
        if task_list_path is not None and not is_blank(task_list_path.value):
            declared = Path(task_list_path.value)
            if not declared.is_absolute():
                self.error(
                    "E_TASK_LIST_PATH_ABSOLUTE",
                    task_list_path.line,
                    "任务清单字段必须是绝对路径",
                )
            elif normalized_path(declared) != normalized_path(self.path):
                self.error(
                    "E_TASK_LIST_PATH_MISMATCH",
                    task_list_path.line,
                    "任务清单字段与实际文件路径不一致",
                )

        self._validate_program_and_docs(fields)

    def _validate_program_and_docs(self, fields: dict[str, FieldValue]) -> None:
        for label, expect_directory in (
            ("实际使用的 ATT", False),
            ("ATT 文档版本", True),
        ):
            field_value = fields.get(label)
            if field_value is None or is_blank(field_value.value):
                continue
            declared_path, identity = split_path_and_identity(field_value.value)
            if declared_path is None or identity is None:
                self.error(
                    "E_PROGRAM_DOCS_IDENTITY",
                    field_value.line,
                    f"“{label}”必须使用“绝对路径；文件或目录内容哈希、Git 提交 ID、"
                    "构建版本等可以核对的版本标识”格式",
                    label,
                )
                continue
            if not declared_path.is_absolute():
                self.error(
                    "E_PROGRAM_DOCS_PATH",
                    field_value.line,
                    f"“{label}”中的路径必须是绝对路径",
                    label,
                )
                continue
            if expect_directory:
                if not declared_path.is_dir():
                    self.error(
                        "E_DOCS_ROOT",
                        field_value.line,
                        "ATT 文档目录不存在或不是目录",
                        label,
                    )
                else:
                    self.docs_root = declared_path.resolve()
            elif not declared_path.is_file():
                self.error(
                    "E_ATT_PROGRAM",
                    field_value.line,
                    "实际使用的 ATT 程序不存在或不是文件",
                    label,
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
                        f"阶段缺少固定 TODO {identifier}",
                        identifier,
                    )
        for identifier, section in ADDITIONAL_FIXED_TODOS.items():
            if identifier not in self.todos:
                self.error(
                    "E_FIXED_TODO_MISSING",
                    self._section_line(section),
                    f"阶段缺少固定 TODO {identifier}",
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
                allow_blank={"上级 TODO", "需要先完成", "结果和证据"},
            )

            checked = todo.checkbox == "x"
            if (todo.state in CHECKED_STATES) != checked:
                self.error(
                    "E_STATUS_CHECKBOX",
                    todo.line,
                    f"勾选框与 TODO 状态 {todo.state} 不一致",
                    todo.identifier,
                )

            task_type = todo.fields.get("任务类型")
            if task_type is not None and task_type.value not in TODO_TYPES:
                self.error(
                    "E_TODO_TYPE",
                    task_type.line,
                    f"未知任务类型“{task_type.value}”",
                    todo.identifier,
                )

            result = todo.fields.get("最近执行结果")
            if result is not None and result.value not in EXECUTION_RESULTS:
                self.error(
                    "E_EXECUTION_RESULT",
                    result.line,
                    f"未知执行结果“{result.value}”",
                    todo.identifier,
                )
            if (
                result is not None
                and result.value == "OutcomeUnknown"
                and todo.state != "BLOCKED"
            ):
                self.error(
                    "E_OUTCOME_UNKNOWN_STATE",
                    result.line,
                    "最近执行结果为 OutcomeUnknown 时，TODO 状态必须设为 BLOCKED",
                    todo.identifier,
                )
            if (
                result is not None
                and todo.state in {"DONE", "N/A"}
                and result.value != "Succeeded"
            ):
                self.error(
                    "E_COMPLETED_RESULT",
                    result.line,
                    f"{todo.state} 的最近执行结果必须是 Succeeded",
                    todo.identifier,
                )
            if (
                result is not None
                and todo.state == "CANCELLED"
                and result.value != "Cancelled"
            ):
                self.error(
                    "E_CANCELLED_RESULT",
                    result.line,
                    "CANCELLED 的最近执行结果必须是 Cancelled",
                    todo.identifier,
                )

            evidence_field = todo.fields.get("结果和证据")
            evidence_refs: set[str] = (
                set(find_refs(EVIDENCE_ID_RE, evidence_field.value))
                if evidence_field is not None
                else set()
            )

            if task_type is not None and task_type.value in {
                "阶段开始前检查",
                "术语检查",
            }:
                self._validate_normative_basis(todo.fields, todo.identifier, todo.line)

            if todo.state == "DONE":
                if evidence_field is None or is_blank(evidence_field.value):
                    self.error(
                        "E_DONE_EVIDENCE",
                        todo.line,
                        "DONE 必须填写结果和证据",
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
                    todo, "不适用依据", require_evidence=True
                )
            elif todo.state == "BLOCKED":
                _ = self._require_conditional_field(todo, "阻塞原因")
                _ = self._require_conditional_field(todo, "已确认情况")
            elif todo.state == "SUPERSEDED":
                replacement = self._require_conditional_field(todo, "替代 TODO")
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

            non_skippable = task_type is not None and task_type.value in {
                "阶段开始前检查",
                "阶段完成检查",
                "术语检查",
            }
            if non_skippable and todo.state in {
                "N/A",
                "SUPERSEDED",
                "CANCELLED",
            }:
                self.error(
                    "E_REQUIRED_CHECK_STATE",
                    todo.line,
                    "阶段开始前检查、阶段完成检查和术语检查的状态只能是 "
                    "TODO、DOING、BLOCKED 或 DONE",
                    todo.identifier,
                )

        for identifier, (
            expected_type,
            expected_parent,
            expected_dependencies,
        ) in FIXED_TODO_EXPECTATIONS.items():
            todo = self.todos.get(identifier)
            if todo is None:
                continue
            task_type = todo.fields.get("任务类型")
            if task_type is not None and task_type.value != expected_type:
                self.error(
                    "E_FIXED_TODO_TYPE",
                    task_type.line,
                    f"{identifier} 的任务类型必须是“{expected_type}”",
                    identifier,
                )
            parent = todo.fields.get("上级 TODO")
            parent_refs = (
                set(find_refs(TODO_ID_RE, parent.value))
                if parent is not None
                else set()
            )
            expected_parent_refs = (
                {expected_parent} if expected_parent is not None else set()
            )
            if parent_refs != expected_parent_refs:
                self.error(
                    "E_FIXED_TODO_PARENT",
                    parent.line if parent is not None else todo.line,
                    f"{identifier} 的上级 TODO 必须是"
                    f"“{expected_parent if expected_parent is not None else '—'}”",
                    identifier,
                )
            dependencies = todo.fields.get("需要先完成")
            dependency_refs = (
                set(find_refs(TODO_ID_RE, dependencies.value))
                if dependencies is not None
                else set()
            )
            if not expected_dependencies.issubset(dependency_refs):
                missing = "、".join(sorted(expected_dependencies - dependency_refs))
                self.error(
                    "E_FIXED_TODO_DEPENDENCY",
                    dependencies.line if dependencies is not None else todo.line,
                    f"{identifier} 缺少固定依赖：{missing}",
                    identifier,
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
            validity = record.fields.get("当前是否适用")
            if validity is not None:
                if validity.value == "是":
                    pass
                elif validity.value.startswith("否（") and validity.value.endswith(
                    "）"
                ):
                    change_refs = find_refs(CHANGE_ID_RE, validity.value)
                    if not change_refs:
                        self.error(
                            "E_EVIDENCE_VALIDITY",
                            validity.line,
                            "不再适用于当前项目的证据必须引用 CHG-*",
                            record.identifier,
                        )
                else:
                    self.error(
                        "E_EVIDENCE_VALIDITY",
                        validity.line,
                        "“当前是否适用”只能填写“是”或“否（CHG-*）”",
                        record.identifier,
                    )

        for record in self.changes.values():
            self._require_fields(
                record.fields, CHANGE_REQUIRED_FIELDS, record.identifier, record.line
            )
            status = record.fields.get("处理状态")
            if status is not None:
                if status.value not in CHANGE_STATES:
                    self.error(
                        "E_CHANGE_STATUS",
                        status.line,
                        "处理状态只能填写“待处理”或“已完成”",
                        record.identifier,
                    )
                else:
                    self.change_states[record.identifier] = status.value

            config = record.fields.get("是否修改项目配置")
            if config is not None and config.value not in {"是", "否"}:
                self.error(
                    "E_CONFIG_CHANGE_VALUE",
                    config.line,
                    "是否修改项目配置只能填写“是”或“否”",
                    record.identifier,
                )
            if config is not None and config.value == "是":
                self._require_fields(
                    record.fields,
                    CONFIG_CHANGE_FIELDS,
                    record.identifier,
                    record.line,
                )
                self._validate_normative_basis(
                    record.fields,
                    record.identifier,
                    record.line,
                    missing_already_reported=True,
                )

        known_evidence = set(self.evidence)
        for todo in self.todos.values():
            for label in ("结果和证据", "不适用依据", "取消依据"):
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
                for identifier in refs:
                    evidence = self.evidence.get(identifier)
                    supported = (
                        evidence.fields.get("支持或否定")
                        if evidence is not None
                        else None
                    )
                    supported_changes = (
                        set(find_refs(CHANGE_ID_RE, supported.value))
                        if supported is not None
                        else set()
                    )
                    if record.identifier not in supported_changes:
                        self.error(
                            "E_CHANGE_TRIGGER_BACKLINK",
                            supported.line if supported is not None else trigger.line,
                            f"触发证据 {identifier} 的“支持或否定”必须列出"
                            f" {record.identifier}",
                            record.identifier,
                        )

        known_todos = set(self.todos)
        known_changes = set(self.changes)
        for record in self.evidence.values():
            supported = record.fields.get("支持或否定")
            if supported is not None:
                todo_refs = find_refs(TODO_ID_RE, supported.value)
                change_refs = find_refs(CHANGE_ID_RE, supported.value)
                final_refs = find_refs(FINAL_ID_RE, supported.value)
                if not todo_refs and not change_refs and not final_refs:
                    self.error(
                        "E_EVIDENCE_TARGET",
                        supported.line,
                        "证据必须指向 TODO、CHG 或 FINAL-* 检查项",
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
            validity = record.fields.get("当前是否适用")
            if validity is not None:
                self._validate_known_refs(
                    find_refs(CHANGE_ID_RE, validity.value),
                    known_changes,
                    "E_UNKNOWN_CHANGE",
                    validity.line,
                    record.identifier,
                )

        for record in self.changes.values():
            affected = record.fields.get("受影响范围")
            affected_refs = (
                set(find_refs(TODO_ID_RE, affected.value))
                if affected is not None
                else set()
            )
            recheck = record.fields.get("重新检查 TODO")
            recheck_refs = (
                set(find_refs(TODO_ID_RE, recheck.value))
                if recheck is not None
                else set()
            )
            self.change_affected_todos[record.identifier] = affected_refs
            self.change_recheck_todos[record.identifier] = recheck_refs

            if not affected_refs:
                self.error(
                    "E_CHANGE_AFFECTED_TODO",
                    affected.line if affected is not None else record.line,
                    "“受影响范围”必须引用至少一个已有 TODO",
                    record.identifier,
                )
            if not recheck_refs:
                self.error(
                    "E_CHANGE_RECHECK_TODO",
                    recheck.line if recheck is not None else record.line,
                    "“重新检查 TODO”必须引用至少一个已有 TODO",
                    record.identifier,
                )

            for label in (
                "受影响范围",
                "重新检查 TODO",
                "修改 TODO",
                "修改前检查 TODO",
                "修改后检查 TODO",
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

            self._validate_change_tasks(record)
            config = record.fields.get("是否修改项目配置")
            if config is not None and config.value == "是":
                self._validate_config_change_tasks(record)

        self._validate_terminal_todo_evidence()

    def _validate_normative_basis(
        self,
        fields: dict[str, FieldValue],
        object_id: str,
        fallback_line: int,
        missing_already_reported: bool = False,
    ) -> None:
        basis = fields.get("参考文档")
        if basis is None:
            if not missing_already_reported:
                self.error(
                    "E_REQUIRED_FIELD",
                    fallback_line,
                    "缺少必填字段“参考文档”",
                    object_id,
                )
            return
        if is_blank(basis.value):
            if not missing_already_reported:
                self.error(
                    "E_EMPTY_FIELD",
                    basis.line,
                    "必填字段“参考文档”不能为空或为占位符",
                    object_id,
                )
            return
        references = markdown_path_references(basis.value)
        if not references:
            self.error(
                "E_NORMATIVE_BASIS",
                basis.line,
                "“参考文档”必须至少包含一个绝对 .md 文件路径",
                object_id,
            )
            return
        for path_text, anchor, whole_document in references:
            path = Path(path_text)
            if not path.is_absolute():
                self.error(
                    "E_REFERENCE_PATH",
                    basis.line,
                    f"参考文档必须使用绝对路径：{path_text}",
                    object_id,
                )
                continue
            if not path.is_file():
                self.error(
                    "E_REFERENCE_MISSING",
                    basis.line,
                    f"参考文档不存在：{path_text}",
                    object_id,
                )
                continue
            if self.docs_root is not None and not path_is_within(
                path.resolve(), self.docs_root
            ):
                self.error(
                    "E_REFERENCE_OUTSIDE_DOCS",
                    basis.line,
                    f"参考文档不在本任务的 ATT 文档目录内：{path_text}",
                    object_id,
                )
            if anchor is None and not whole_document:
                self.error(
                    "E_REFERENCE_SECTION",
                    basis.line,
                    f"参考文档必须写明 #章节或“全文”：{path_text}",
                    object_id,
                )
            elif anchor is not None:
                try:
                    anchors = markdown_heading_anchors(path.read_text(encoding="utf-8"))
                except (OSError, UnicodeError) as error:
                    self.error(
                        "E_REFERENCE_READ",
                        basis.line,
                        f"无法读取参考文档 {path_text}：{error}",
                        object_id,
                    )
                    continue
                normalized_anchor = anchor.removeprefix("#")
                if normalized_anchor not in anchors:
                    self.error(
                        "E_REFERENCE_SECTION",
                        basis.line,
                        f"参考文档中不存在章节 #{normalized_anchor}：{path_text}",
                        object_id,
                    )

    def _validate_terminal_todo_evidence(self) -> None:
        terminal_states = {"DONE", "N/A", "CANCELLED"}
        evidence_labels = ("结果和证据", "不适用依据", "取消依据")
        affected_by_change: dict[str, set[str]] = defaultdict(set)
        for change_id, todo_ids in self.change_affected_todos.items():
            for todo_id in todo_ids:
                affected_by_change[todo_id].add(change_id)

        current_todos = {
            identifier
            for summary in self.phase_summaries.values()
            for identifier in (
                summary.current_task,
                summary.start_check,
                summary.completion_check,
                summary.terminology_check,
            )
            if identifier != "—"
        }
        affected_todos = {
            identifier
            for identifiers in self.change_affected_todos.values()
            for identifier in identifiers
        }
        current_todos.update(
            identifier
            for identifiers in self.change_recheck_todos.values()
            for identifier in identifiers
            if identifier not in affected_todos
        )

        for todo in self.todos.values():
            if todo.state not in terminal_states:
                continue
            references: set[str] = set()
            for label in evidence_labels:
                field_value = todo.fields.get(label)
                if field_value is not None:
                    references.update(find_refs(EVIDENCE_ID_RE, field_value.value))
            for identifier in references:
                record = self.evidence.get(identifier)
                if record is None:
                    continue
                applicability = record.fields.get("当前是否适用")
                if applicability is None or applicability.value != "是":
                    change_refs = (
                        set(find_refs(CHANGE_ID_RE, applicability.value))
                        if applicability is not None
                        else set()
                    )
                    allowed_history = (
                        todo.identifier not in current_todos
                        and bool(change_refs)
                        and change_refs.issubset(
                            affected_by_change.get(todo.identifier, set())
                        )
                    )
                    if not allowed_history:
                        self.error(
                            "E_TODO_EVIDENCE_APPLICABILITY",
                            applicability.line
                            if applicability is not None
                            else record.line,
                            f"{todo.state} 引用的证据 {identifier} 当前不适用于"
                            f" {todo.identifier}",
                            todo.identifier,
                        )
                supported = record.fields.get("支持或否定")
                supported_todos: set[str] = (
                    set(find_refs(TODO_ID_RE, supported.value))
                    if supported is not None
                    else set()
                )
                if todo.identifier not in supported_todos:
                    self.error(
                        "E_TODO_EVIDENCE_BACKLINK",
                        supported.line if supported is not None else record.line,
                        f"证据 {identifier} 的“支持或否定”字段没有列出 {todo.identifier}",
                        todo.identifier,
                    )

    def _validate_change_tasks(self, record: Record) -> None:
        affected = self.change_affected_todos.get(record.identifier, set())
        rechecks = self.change_recheck_todos.get(record.identifier, set())
        overlap = affected & rechecks
        if overlap:
            field_value = record.fields.get("重新检查 TODO")
            self.error(
                "E_CHANGE_RECHECK_REUSE",
                field_value.line if field_value is not None else record.line,
                "受影响的旧 TODO 不能同时充当新建的重新检查 TODO："
                + "、".join(sorted(overlap)),
                record.identifier,
            )
        affected_phases = {
            self.todos[identifier].prefix
            for identifier in affected
            if identifier in self.todos
        }
        recheck_phases = {
            self.todos[identifier].prefix
            for identifier in rechecks
            if identifier in self.todos
        }
        missing_phases = affected_phases - recheck_phases
        if missing_phases:
            field_value = record.fields.get("重新检查 TODO")
            self.error(
                "E_CHANGE_PHASE_COVERAGE",
                field_value.line if field_value is not None else record.line,
                "重新检查 TODO 没有覆盖受影响阶段："
                + "、".join(sorted(missing_phases)),
                record.identifier,
            )

        affected_terminology = any(
            identifier in self.todos
            and self.todos[identifier].fields.get("任务类型") is not None
            and self.todos[identifier].fields["任务类型"].value == "术语检查"
            for identifier in affected
        )
        if affected_terminology:
            terminology_rechecks = [
                identifier
                for identifier in rechecks
                if (todo := self.todos.get(identifier)) is not None
                and todo.prefix == "TRN"
                and (task_type := todo.fields.get("任务类型")) is not None
                and task_type.value == "术语检查"
            ]
            if len(terminology_rechecks) != 1:
                field_value = record.fields.get("重新检查 TODO")
                self.error(
                    "E_CHANGE_TERMINOLOGY_CHECK",
                    field_value.line if field_value is not None else record.line,
                    "术语检查失效后，变更必须新建且只新建一个“术语检查”TODO",
                    record.identifier,
                )

        for prefix in affected_phases:
            for expected_type in (
                "阶段任务",
                "阶段开始前检查",
                "阶段完成检查",
            ):
                matching = [
                    identifier
                    for identifier in rechecks
                    if (todo := self.todos.get(identifier)) is not None
                    and todo.prefix == prefix
                    and (task_type := todo.fields.get("任务类型")) is not None
                    and task_type.value == expected_type
                ]
                if len(matching) != 1:
                    field_value = record.fields.get("重新检查 TODO")
                    self.error(
                        "E_CHANGE_PHASE_CHECKS",
                        field_value.line if field_value is not None else record.line,
                        f"{record.identifier} 必须为 {prefix} 阶段新建且只新建一个"
                        f"“{expected_type}”TODO",
                        record.identifier,
                    )

            affected_numbers = [
                self.todos[identifier].number
                for identifier in affected
                if identifier in self.todos and self.todos[identifier].prefix == prefix
            ]
            if affected_numbers:
                newest_affected = max(affected_numbers)
                reused_numbers = sorted(
                    identifier
                    for identifier in rechecks
                    if identifier in self.todos
                    and self.todos[identifier].prefix == prefix
                    and self.todos[identifier].number <= newest_affected
                )
                if reused_numbers:
                    field_value = record.fields.get("重新检查 TODO")
                    self.error(
                        "E_CHANGE_RECHECK_ORDER",
                        field_value.line if field_value is not None else record.line,
                        "重新检查 TODO 的编号必须晚于同阶段受影响的旧 TODO："
                        + "、".join(reused_numbers),
                        record.identifier,
                    )

        for identifier in rechecks:
            todo = self.todos.get(identifier)
            if todo is None:
                continue
            if identifier in FIXED_TODO_EXPECTATIONS:
                self.error(
                    "E_CHANGE_FIXED_RECHECK",
                    todo.line,
                    f"{identifier} 是固定 TODO，不能作为新建的重新检查 TODO",
                    identifier,
                )
            owner = todo.fields.get("所属变更")
            owner_refs = (
                set(find_refs(CHANGE_ID_RE, owner.value))
                if owner is not None
                else set()
            )
            if owner_refs != {record.identifier}:
                self.error(
                    "E_CHANGE_TODO_OWNER",
                    owner.line if owner is not None else todo.line,
                    f"{identifier} 的“所属变更”必须只填写 {record.identifier}",
                    identifier,
                )

        next_field = record.fields.get("下一步")
        next_refs = (
            set(find_refs(TODO_ID_RE, next_field.value))
            if next_field is not None
            else set()
        )
        if len(next_refs) != 1:
            self.error(
                "E_CHANGE_NEXT",
                next_field.line if next_field is not None else record.line,
                "变更的“下一步”必须恰好引用一个 TODO",
                record.identifier,
            )
        elif not next_refs.issubset(rechecks):
            self.error(
                "E_CHANGE_NEXT",
                next_field.line if next_field is not None else record.line,
                "变更的“下一步”必须属于重新检查 TODO",
                record.identifier,
            )
        elif self.change_states.get(record.identifier) == "待处理":
            next_todo = self.todos.get(next(iter(next_refs)))
            if next_todo is not None and next_todo.state not in OPEN_STATES:
                self.error(
                    "E_CHANGE_NEXT",
                    next_field.line if next_field is not None else record.line,
                    "待处理变更的“下一步”必须指向尚未完成的重新检查 TODO",
                    record.identifier,
                )

        successful = {
            identifier
            for identifier in rechecks
            if (todo := self.todos.get(identifier)) is not None
            and (
                todo.state == "DONE"
                or (
                    todo.state == "N/A"
                    and todo.fields.get("任务类型") is not None
                    and todo.fields["任务类型"].value == "阶段任务"
                )
            )
        }
        status = self.change_states.get(record.identifier)
        if status == "待处理" and rechecks and successful == rechecks:
            self.error(
                "E_CHANGE_STATUS_MISMATCH",
                record.fields["处理状态"].line,
                "所有重新检查 TODO 已完成，处理状态应更新为“已完成”",
                record.identifier,
            )
        elif status == "已完成" and successful != rechecks:
            missing = "、".join(sorted(rechecks - successful))
            self.error(
                "E_CHANGE_STATUS_MISMATCH",
                record.fields["处理状态"].line,
                f"处理状态为“已完成”，但以下 TODO 尚未成功完成：{missing}",
                record.identifier,
            )

    def _validate_config_change_tasks(self, record: Record) -> None:
        tasks_by_role: dict[str, str] = {}
        rechecks = self.change_recheck_todos.get(record.identifier, set())
        expected_types = {
            "修改前检查 TODO": "调查",
            "修改 TODO": "执行",
            "修改后检查 TODO": "调查",
        }
        for label, expected_type in expected_types.items():
            field_value = record.fields.get(label)
            if field_value is None:
                continue
            refs = set(find_refs(TODO_ID_RE, field_value.value))
            if len(refs) != 1:
                self.error(
                    "E_CONFIG_TASK_REF",
                    field_value.line,
                    f"“{label}”必须恰好引用一个 TODO ID",
                    record.identifier,
                )
                continue
            identifier = next(iter(refs))
            tasks_by_role[label] = identifier
            todo = self.todos.get(identifier)
            if todo is None:
                continue
            previous_change = self.config_task_changes.get(identifier)
            if previous_change is not None and previous_change != record.identifier:
                self.error(
                    "E_CONFIG_TASK_REUSE",
                    field_value.line,
                    f"{identifier} 已由配置变更 {previous_change} 使用，不能再次用于"
                    f" {record.identifier}",
                    record.identifier,
                )
            else:
                self.config_task_changes[identifier] = record.identifier
            if identifier in FIXED_TODO_EXPECTATIONS:
                self.error(
                    "E_CONFIG_FIXED_TODO",
                    field_value.line,
                    f"{identifier} 是固定 TODO，不能用于一次具体配置修改",
                    record.identifier,
                )
            if identifier not in rechecks:
                self.error(
                    "E_CONFIG_RECHECK",
                    field_value.line,
                    f"{identifier} 必须列入“重新检查 TODO”",
                    record.identifier,
                )
            owner = todo.fields.get("所属变更")
            owner_refs = (
                set(find_refs(CHANGE_ID_RE, owner.value))
                if owner is not None
                else set()
            )
            if owner_refs != {record.identifier}:
                self.error(
                    "E_CONFIG_TODO_OWNER",
                    owner.line if owner is not None else todo.line,
                    f"{identifier} 必须只属于 {record.identifier}",
                    identifier,
                )
            task_type = todo.fields.get("任务类型")
            if task_type is None or task_type.value != expected_type:
                self.error(
                    "E_CONFIG_TASK_TYPE",
                    task_type.line if task_type is not None else todo.line,
                    f"“{label}”引用的 {identifier} 必须使用任务类型“{expected_type}”",
                    identifier,
                )
            if todo.state in CHECKED_STATES and todo.state != "DONE":
                self.error(
                    "E_CONFIG_TASK_STATE",
                    todo.line,
                    f"配置修改步骤 {identifier} 不能以 {todo.state} 跳过",
                    identifier,
                )
            result = todo.fields.get("最近执行结果")
            if self.change_states.get(record.identifier) == "已完成" and (
                todo.state != "DONE" or result is None or result.value != "Succeeded"
            ):
                self.error(
                    "E_CONFIG_TASK_STATE",
                    todo.line,
                    f"配置变更完成前，{identifier} 必须为 DONE/Succeeded",
                    identifier,
                )

        if len(set(tasks_by_role.values())) != len(tasks_by_role):
            self.error(
                "E_CONFIG_TASK_SEPARATION",
                record.line,
                "修改前检查、修改和修改后检查必须使用三个不同 TODO",
                record.identifier,
            )

        before = tasks_by_role.get("修改前检查 TODO")
        modify = tasks_by_role.get("修改 TODO")
        after = tasks_by_role.get("修改后检查 TODO")
        if before is not None and modify is not None:
            dependencies = self.todos.get(modify)
            dependency_refs = (
                set(
                    find_refs(
                        TODO_ID_RE,
                        dependencies.fields["需要先完成"].value,
                    )
                )
                if dependencies is not None and "需要先完成" in dependencies.fields
                else set()
            )
            if before not in dependency_refs:
                self.error(
                    "E_CONFIG_TASK_ORDER",
                    self.todos[modify].line if modify in self.todos else record.line,
                    f"{modify} 必须依赖修改前检查 {before}",
                    record.identifier,
                )
        if modify is not None and after is not None:
            dependencies = self.todos.get(after)
            dependency_refs = (
                set(
                    find_refs(
                        TODO_ID_RE,
                        dependencies.fields["需要先完成"].value,
                    )
                )
                if dependencies is not None and "需要先完成" in dependencies.fields
                else set()
            )
            if modify not in dependency_refs:
                self.error(
                    "E_CONFIG_TASK_ORDER",
                    self.todos[after].line if after in self.todos else record.line,
                    f"{after} 必须依赖修改 TODO {modify}",
                    record.identifier,
                )

    def _validate_phase_summaries(self) -> None:
        if not self.phase_summary_header_seen:
            self.error(
                "E_PHASE_SUMMARY_HEADER",
                self._section_line("阶段总览与下一步"),
                "阶段总览缺少当前模板规定的表头",
            )
        if set(self.phase_summaries) != set(PHASE_NAMES):
            self.error(
                "E_PHASE_SUMMARY_SET",
                self._section_line("阶段总览与下一步"),
                "阶段总览必须恰好包含解包、提取、翻译、写回、封包五行",
            )

        pending_by_prefix: dict[str, set[str]] = defaultdict(set)
        affecting_by_prefix: dict[str, list[str]] = defaultdict(list)
        affected_todos = {
            identifier
            for identifiers in self.change_affected_todos.values()
            for identifier in identifiers
        }
        for change_id, affected_todos in self.change_affected_todos.items():
            prefixes = {
                self.todos[identifier].prefix
                for identifier in affected_todos
                if identifier in self.todos
            }
            for prefix in prefixes:
                affecting_by_prefix[prefix].append(change_id)
                if self.change_states.get(change_id) == "待处理":
                    pending_by_prefix[prefix].add(change_id)

        for summary in self.phase_summaries.values():
            if summary.state not in RESPONSIBILITY_STATES:
                self.error(
                    "E_PHASE_STATE",
                    summary.line,
                    f"阶段状态“{summary.state}”不是任务清单支持的状态",
                    summary.name,
                )
            prefix = PHASE_NAMES[summary.name]

            current_objects: dict[str, Todo | None] = {}
            for label, identifier, expected_type in (
                ("当前阶段任务", summary.current_task, "阶段任务"),
                ("当前开始前检查", summary.start_check, "阶段开始前检查"),
                ("当前完成检查", summary.completion_check, "阶段完成检查"),
            ):
                todo = self.todos.get(identifier)
                current_objects[label] = todo
                if todo is None:
                    self.error(
                        "E_PHASE_CURRENT_REF",
                        summary.line,
                        f"{label}引用了不存在的 TODO {identifier}",
                        summary.name,
                    )
                elif todo.prefix != prefix:
                    self.error(
                        "E_PHASE_CURRENT_PREFIX",
                        summary.line,
                        f"{label} {identifier} 不属于{summary.name}阶段",
                        summary.name,
                    )
                else:
                    task_type = todo.fields.get("任务类型")
                    if task_type is None or task_type.value != expected_type:
                        self.error(
                            "E_PHASE_CURRENT_TYPE",
                            task_type.line if task_type is not None else todo.line,
                            f"{label} {identifier} 的任务类型必须是“{expected_type}”",
                            identifier,
                        )

            current_task = current_objects.get("当前阶段任务")
            start_check = current_objects.get("当前开始前检查")
            completion_check = current_objects.get("当前完成检查")
            terminology_check: Todo | None = None
            if prefix == "TRN":
                terminology_check = self.todos.get(summary.terminology_check)
                if terminology_check is None:
                    self.error(
                        "E_PHASE_TERMINOLOGY_REF",
                        summary.line,
                        f"当前术语检查引用了不存在的 TODO {summary.terminology_check}",
                        summary.name,
                    )
                elif terminology_check.prefix != "TRN":
                    self.error(
                        "E_PHASE_TERMINOLOGY_REF",
                        summary.line,
                        f"当前术语检查 {summary.terminology_check} 不属于翻译阶段",
                        summary.name,
                    )
                else:
                    task_type = terminology_check.fields.get("任务类型")
                    if task_type is None or task_type.value != "术语检查":
                        self.error(
                            "E_PHASE_TERMINOLOGY_TYPE",
                            task_type.line
                            if task_type is not None
                            else terminology_check.line,
                            f"{summary.terminology_check} 的任务类型必须是“术语检查”",
                            summary.terminology_check,
                        )
            elif summary.terminology_check != "—":
                self.error(
                    "E_PHASE_TERMINOLOGY_REF",
                    summary.line,
                    "只有翻译阶段可以填写“当前术语检查”；其他阶段必须写“—”",
                    summary.name,
                )
            if current_task is not None and summary.state != current_task.state:
                self.error(
                    "E_PHASE_STATE_MISMATCH",
                    summary.line,
                    f"阶段状态 {summary.state} 与当前阶段任务 "
                    f"{current_task.identifier} [{current_task.state}] 不一致",
                    summary.name,
                )

            if current_task is not None:
                dependencies = self._todo_dependency_refs(current_task)
                required = {summary.start_check, summary.completion_check}
                if not required.issubset(dependencies):
                    self.error(
                        "E_PHASE_TASK_DEPENDENCY",
                        current_task.line,
                        f"{current_task.identifier} 必须依赖当前开始前检查和当前完成检查",
                        current_task.identifier,
                    )

            terminal_phase = summary.state in {"DONE", "N/A"}
            if terminal_phase:
                for label, todo in (
                    ("当前开始前检查", start_check),
                    ("当前完成检查", completion_check),
                    ("当前术语检查", terminology_check),
                ):
                    if todo is not None and todo.state != "DONE":
                        self.error(
                            "E_PHASE_CHECK_OPEN",
                            summary.line,
                            f"阶段已经 {summary.state}，但{label} "
                            f"{todo.identifier} 尚未 DONE",
                            summary.name,
                        )
                open_todos = sorted(
                    todo.identifier
                    for todo in self.todos.values()
                    if todo.prefix == prefix and todo.state in OPEN_STATES
                )
                if open_todos:
                    self.error(
                        "E_PHASE_OPEN_TODO",
                        summary.line,
                        f"阶段已经 {summary.state}，但仍有未完成 TODO："
                        + "、".join(open_todos),
                        summary.name,
                    )
                if summary.next_todo != summary.completion_check:
                    self.error(
                        "E_PHASE_NEXT",
                        summary.line,
                        "已完成阶段的“下一步”必须指向当前完成检查",
                        summary.name,
                    )
            else:
                next_todo = self.todos.get(summary.next_todo)
                if next_todo is None:
                    self.error(
                        "E_PHASE_NEXT",
                        summary.line,
                        f"下一步引用了不存在的 TODO {summary.next_todo}",
                        summary.name,
                    )
                elif next_todo.prefix != prefix:
                    self.error(
                        "E_PHASE_NEXT",
                        summary.line,
                        f"下一步 {summary.next_todo} 不属于{summary.name}阶段",
                        summary.name,
                    )
                elif next_todo.state not in OPEN_STATES:
                    self.error(
                        "E_PHASE_NEXT",
                        summary.line,
                        f"未完成阶段的下一步 {summary.next_todo} 必须仍未完成",
                        summary.name,
                    )

            if completion_check is not None:
                dependency_closure = self._dependency_closure(
                    completion_check.identifier
                )
                if summary.start_check not in dependency_closure:
                    self.error(
                        "E_COMPLETION_CHECK_START",
                        completion_check.line,
                        f"{completion_check.identifier} 必须依赖当前开始前检查"
                        f" {summary.start_check}",
                        completion_check.identifier,
                    )
                required_work = {
                    todo.identifier
                    for todo in self.todos.values()
                    if todo.prefix == prefix
                    and todo.fields.get("任务类型") is not None
                    and todo.fields["任务类型"].value in {"术语检查", "执行", "调查"}
                    and todo.state not in {"SUPERSEDED", "CANCELLED"}
                    and todo.identifier not in affected_todos
                }
                if not required_work.issubset(dependency_closure):
                    missing = "、".join(sorted(required_work - dependency_closure))
                    self.error(
                        "E_COMPLETION_CHECK_COVERAGE",
                        completion_check.line,
                        f"当前完成检查没有依赖以下术语检查、执行或调查 TODO：{missing}",
                        completion_check.identifier,
                    )

            listed_changes_text = summary.pending_changes.strip()
            if listed_changes_text == "—":
                listed_changes: set[str] = set()
            else:
                listed_changes = set(find_refs(CHANGE_ID_RE, listed_changes_text))
                if not listed_changes:
                    self.error(
                        "E_PHASE_CHANGE_LIST",
                        summary.line,
                        "“待重新验证的变更”必须写“—”或列出 CHG-*",
                        summary.name,
                    )
            self._validate_known_refs(
                listed_changes,
                set(self.changes),
                "E_UNKNOWN_CHANGE",
                summary.line,
                summary.name,
            )
            expected_pending = pending_by_prefix.get(prefix, set())
            if listed_changes != expected_pending:
                self.error(
                    "E_PHASE_CHANGE_LIST",
                    summary.line,
                    "“待重新验证的变更”与变更记录不一致；应为："
                    + (
                        "、".join(sorted(expected_pending)) if expected_pending else "—"
                    ),
                    summary.name,
                )
            if expected_pending and terminal_phase:
                self.error(
                    "E_PHASE_PENDING_CHANGE",
                    summary.line,
                    "还有待重新验证的变更时，阶段不能标为 DONE 或 N/A",
                    summary.name,
                )
            if len(expected_pending) > 1:
                self.error(
                    "E_OVERLAPPING_CHANGES",
                    summary.line,
                    "同一阶段前一个变更完成前，不能开始另一个变更",
                    summary.name,
                )

            affecting = affecting_by_prefix.get(prefix, [])
            if affecting:
                latest_change = max(affecting, key=change_number)
                latest_rechecks = self.change_recheck_todos.get(latest_change, set())
                current_ids = {
                    summary.current_task,
                    summary.start_check,
                    summary.completion_check,
                }
                if not current_ids.issubset(latest_rechecks):
                    self.error(
                        "E_PHASE_CURRENT_CHANGE",
                        summary.line,
                        f"阶段总览当前三个 TODO 必须属于最新变更 {latest_change} 的"
                        "“重新检查 TODO”",
                        summary.name,
                    )

            if prefix == "TRN":
                terminology_changes = [
                    change_id
                    for change_id, affected in self.change_affected_todos.items()
                    if any(
                        identifier in self.todos
                        and self.todos[identifier].fields.get("任务类型") is not None
                        and self.todos[identifier].fields["任务类型"].value
                        == "术语检查"
                        for identifier in affected
                    )
                ]
                if terminology_changes:
                    latest_terminology_change = max(
                        terminology_changes, key=change_number
                    )
                    if summary.terminology_check not in self.change_recheck_todos.get(
                        latest_terminology_change, set()
                    ):
                        self.error(
                            "E_PHASE_CURRENT_TERMINOLOGY_CHANGE",
                            summary.line,
                            "当前术语检查必须指向最新术语变更 "
                            f"{latest_terminology_change} 新建的术语检查 TODO",
                            summary.name,
                        )
                elif summary.terminology_check != "TRN-004":
                    self.error(
                        "E_PHASE_CURRENT_TERMINOLOGY_CHANGE",
                        summary.line,
                        "没有术语变更时，当前术语检查必须为 TRN-004",
                        summary.name,
                    )

            phase_order = list(PHASE_NAMES)
            phase_index = phase_order.index(summary.name)
            if phase_index > 0 and start_check is not None:
                previous_summary = self.phase_summaries.get(
                    phase_order[phase_index - 1]
                )
                if (
                    previous_summary is not None
                    and previous_summary.completion_check
                    not in self._dependency_closure(start_check.identifier)
                ):
                    self.error(
                        "E_PHASE_START_DEPENDENCY",
                        start_check.line,
                        f"{start_check.identifier} 必须依赖前一阶段当前完成检查 "
                        f"{previous_summary.completion_check}",
                        start_check.identifier,
                    )

    def _todo_dependency_refs(self, todo: Todo) -> set[str]:
        field_value = todo.fields.get("需要先完成")
        if field_value is None:
            return set()
        return set(find_refs(TODO_ID_RE, field_value.value))

    def _dependency_closure(self, identifier: str) -> set[str]:
        closure: set[str] = set()
        pending = [identifier]
        while pending:
            current = pending.pop()
            todo = self.todos.get(current)
            if todo is None:
                continue
            for dependency in self._todo_dependency_refs(todo):
                if dependency not in closure:
                    closure.add(dependency)
                    pending.append(dependency)
        return closure

    def _validate_relationships(self) -> None:
        known_todos = set(self.todos)
        parent_graph: dict[str, set[str]] = defaultdict(set)
        dependency_graph: dict[str, set[str]] = defaultdict(set)
        replacement_graph: dict[str, set[str]] = defaultdict(set)

        for todo in self.todos.values():
            parent = todo.fields.get("上级 TODO")
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
                            "上级 TODO 不能引用自身",
                            todo.identifier,
                        )
                    else:
                        parent_graph[todo.identifier].add(target)

            dependency = todo.fields.get("需要先完成")
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
                            "需要先完成的 TODO 不能引用自身",
                            todo.identifier,
                        )
                    else:
                        dependency_graph[todo.identifier].add(target)

            replacement = todo.fields.get("替代 TODO")
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
                            "替代 TODO 不能引用自身",
                            todo.identifier,
                        )
                    else:
                        replacement_graph[todo.identifier].add(target)

            next_field = todo.fields.get("下一步")
            if next_field is not None:
                refs = find_refs(TODO_ID_RE, next_field.value)
                if len(refs) != 1:
                    self.error(
                        "E_TODO_NEXT",
                        next_field.line,
                        "TODO 的“下一步”必须恰好引用一个 TODO ID",
                        todo.identifier,
                    )
                self._validate_known_refs(
                    refs,
                    known_todos,
                    "E_UNKNOWN_TODO",
                    next_field.line,
                    todo.identifier,
                )
                if (
                    len(refs) == 1
                    and refs[0] in self.todos
                    and todo.state in OPEN_STATES
                ):
                    next_todo = self.todos[refs[0]]
                    if next_todo.prefix != todo.prefix:
                        self.error(
                            "E_TODO_NEXT_PHASE",
                            next_field.line,
                            f"未完成 TODO 的下一步必须留在 {todo.prefix} 阶段",
                            todo.identifier,
                        )
                    elif next_todo.state not in OPEN_STATES:
                        self.error(
                            "E_TODO_NEXT_STATE",
                            next_field.line,
                            "未完成 TODO 的下一步必须指向尚未完成的 TODO",
                            todo.identifier,
                        )

            change_owner = todo.fields.get("所属变更")
            if change_owner is not None:
                self._validate_known_refs(
                    find_refs(CHANGE_ID_RE, change_owner.value),
                    set(self.changes),
                    "E_UNKNOWN_CHANGE",
                    change_owner.line,
                    todo.identifier,
                )

        self._report_cycles(parent_graph, "E_PARENT_CYCLE", "上级 TODO")
        self._report_cycles(dependency_graph, "E_DEPENDENCY_CYCLE", "需要先完成")
        self._report_cycles(replacement_graph, "E_REPLACEMENT_CYCLE", "替代 TODO")
        self._validate_dependency_states()
        self._validate_superseded_replacements()

        summary_fields = self.section_fields.get("阶段总览与下一步", {})
        global_next = summary_fields.get("全局下一步")
        if global_next is None:
            self.error(
                "E_GLOBAL_NEXT",
                self._section_line("阶段总览与下一步"),
                "缺少“全局下一步”",
            )
        else:
            refs = find_refs(TODO_ID_RE, global_next.value)
            if len(refs) != 1:
                self.error(
                    "E_GLOBAL_NEXT",
                    global_next.line,
                    "全局下一步必须恰好引用一个 TODO ID",
                )
            else:
                self._validate_known_refs(
                    refs,
                    known_todos,
                    "E_UNKNOWN_TODO",
                    global_next.line,
                    "全局下一步",
                )
                expected_next = self._expected_global_next()
                actual_next = refs[0]
                if expected_next is not None and actual_next != expected_next:
                    self.error(
                        "E_GLOBAL_NEXT",
                        global_next.line,
                        f"全局下一步应为 {expected_next}，实际为 {actual_next}",
                    )
                elif (
                    expected_next is not None
                    and actual_next in self.todos
                    and not self._todo_dependencies_satisfied(self.todos[actual_next])
                ):
                    self.error(
                        "E_GLOBAL_NEXT_DEPENDENCY",
                        global_next.line,
                        f"全局下一步 {actual_next} 仍有尚未成功完成的依赖",
                        actual_next,
                    )

                handoff_next = self.section_fields.get("阻塞、剩余工作和交接", {}).get(
                    "下一步"
                )
                if handoff_next is None:
                    self.error(
                        "E_HANDOFF_NEXT",
                        self._section_line("阻塞、剩余工作和交接"),
                        "“阻塞、剩余工作和交接”缺少“下一步”",
                    )
                else:
                    handoff_refs = find_refs(TODO_ID_RE, handoff_next.value)
                    if handoff_refs != [actual_next]:
                        self.error(
                            "E_HANDOFF_NEXT",
                            handoff_next.line,
                            "交接部分的“下一步”必须与全局下一步一致",
                        )

        for summary in self.phase_summaries.values():
            next_todo = self.todos.get(summary.next_todo)
            if next_todo is None:
                continue
            next_field = next_todo.fields.get("下一步")
            next_refs = (
                find_refs(TODO_ID_RE, next_field.value)
                if next_field is not None
                else []
            )
            if next_refs != [summary.next_todo]:
                self.error(
                    "E_PHASE_TODO_NEXT",
                    next_field.line if next_field is not None else next_todo.line,
                    f"阶段总览指向 {summary.next_todo} 时，该 TODO 的“下一步”也必须"
                    "指向自身",
                    summary.next_todo,
                )

    def _validate_dependency_states(self) -> None:
        for todo in self.todos.values():
            task_type = todo.fields.get("任务类型")
            requires_completed_dependencies = todo.state in {"DONE", "N/A"} or (
                todo.state == "DOING"
                and task_type is not None
                and task_type.value != "阶段任务"
            )
            if not requires_completed_dependencies:
                continue
            incomplete = sorted(
                identifier
                for identifier in self._todo_dependency_refs(todo)
                if identifier in self.todos
                and self.todos[identifier].state not in {"DONE", "N/A"}
            )
            if incomplete:
                self.error(
                    "E_DEPENDENCY_OPEN",
                    todo.fields["需要先完成"].line,
                    f"{todo.identifier} 不能处于 {todo.state}；以下依赖尚未成功完成："
                    + "、".join(incomplete),
                    todo.identifier,
                )

    def _todo_dependencies_satisfied(self, todo: Todo) -> bool:
        return all(
            identifier in self.todos and self.todos[identifier].state in {"DONE", "N/A"}
            for identifier in self._todo_dependency_refs(todo)
        )

    def _validate_superseded_replacements(self) -> None:
        for todo in self.todos.values():
            if todo.state != "SUPERSEDED":
                continue
            replacement = todo.fields.get("替代 TODO")
            replacement_refs = (
                set(find_refs(TODO_ID_RE, replacement.value))
                if replacement is not None
                else set()
            )
            matching_changes = [
                change_id
                for change_id, affected in self.change_affected_todos.items()
                if todo.identifier in affected
                and replacement_refs
                and replacement_refs.issubset(
                    self.change_recheck_todos.get(change_id, set())
                )
            ]
            if len(matching_changes) != 1:
                self.error(
                    "E_SUPERSEDED_CHANGE",
                    replacement.line if replacement is not None else todo.line,
                    "SUPERSEDED 的替代 TODO 必须来自一个明确影响旧 TODO 的 CHG-*",
                    todo.identifier,
                )
                continue
            change_id = matching_changes[0]
            for identifier in replacement_refs:
                target = self.todos.get(identifier)
                if target is None:
                    continue
                owner = target.fields.get("所属变更")
                owner_refs = (
                    set(find_refs(CHANGE_ID_RE, owner.value))
                    if owner is not None
                    else set()
                )
                if (
                    target.prefix != todo.prefix
                    or target.number <= todo.number
                    or identifier in FIXED_TODO_EXPECTATIONS
                    or owner_refs != {change_id}
                ):
                    self.error(
                        "E_SUPERSEDED_TARGET",
                        target.line,
                        f"{identifier} 必须是同阶段、编号更晚且只属于 {change_id} 的"
                        "新 TODO",
                        todo.identifier,
                    )

    def _expected_global_next(self) -> str | None:
        for phase_name in PHASE_NAMES:
            summary = self.phase_summaries.get(phase_name)
            if summary is not None and summary.state not in {"DONE", "N/A"}:
                return summary.next_todo
        packaging = self.phase_summaries.get("封包")
        return packaging.completion_check if packaging is not None else None

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

        final_fields = self.section_fields.get("最终检查", {})
        self._require_fields(
            final_fields,
            FINAL_FIELDS,
            "最终检查",
            self._section_line("最终检查"),
        )
        declaration = final_fields.get("完成状态")
        if declaration is not None and declaration.value != "完成":
            self.error(
                "E_FINAL_DECLARATION",
                declaration.line,
                "--final 要求“完成状态：完成”",
            )

        contract = self.section_fields.get("任务信息", {})
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
                    f"最终完成时仍有未完成 TODO {todo.identifier} [{todo.state}]",
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

        for identifier, record in self.changes.items():
            if self.change_states.get(identifier) != "已完成":
                self.error(
                    "E_OPEN_CHANGE",
                    record.line,
                    f"最终完成时变更 {identifier} 的处理状态必须是“已完成”",
                    identifier,
                )

        valid_evidence = {
            identifier
            for identifier, record in self.evidence.items()
            if (value := record.fields.get("当前是否适用")) is not None
            and value.value == "是"
        }
        for label, final_id in (
            ("用户目标", "FINAL-GOAL"),
            ("最终文件", "FINAL-OUTPUT"),
            ("必须保持的行为", "FINAL-PRESERVE"),
        ):
            value = final_fields.get(label)
            if value is None:
                continue
            refs = set(find_refs(EVIDENCE_ID_RE, value.value))
            if is_blank(value.value) or not refs:
                self.error(
                    "E_FINAL_EVIDENCE",
                    value.line,
                    f"“{label}”必须引用当前仍适用的 EV-*",
                )
            elif not refs.issubset(valid_evidence):
                unavailable = ", ".join(sorted(refs - valid_evidence))
                self.error(
                    "E_FINAL_EVIDENCE_APPLICABILITY",
                    value.line,
                    f"“{label}”引用了不存在或当前不适用的证据：{unavailable}",
                )
            for evidence_id in refs & valid_evidence:
                evidence = self.evidence[evidence_id]
                supported = evidence.fields.get("支持或否定")
                final_refs = (
                    set(find_refs(FINAL_ID_RE, supported.value))
                    if supported is not None
                    else set()
                )
                if final_id not in final_refs:
                    self.error(
                        "E_FINAL_EVIDENCE_BACKLINK",
                        supported.line if supported is not None else evidence.line,
                        f"证据 {evidence_id} 的“支持或否定”必须列出 {final_id}",
                        final_id,
                    )

        config_changes = {
            identifier
            for identifier, record in self.changes.items()
            if (field_value := record.fields.get("是否修改项目配置")) is not None
            and field_value.value == "是"
        }
        config_check = final_fields.get("配置修改检查")
        if config_check is not None:
            change_refs = set(find_refs(CHANGE_ID_RE, config_check.value))
            evidence_refs = set(find_refs(EVIDENCE_ID_RE, config_check.value))
            if not config_changes:
                if config_check.value != "无配置修改":
                    self.error(
                        "E_FINAL_CONFIG",
                        config_check.line,
                        "任务没有配置修改时必须填写“配置修改检查：无配置修改”",
                    )
            else:
                if change_refs != config_changes:
                    self.error(
                        "E_FINAL_CONFIG",
                        config_check.line,
                        "配置修改检查必须列出全部配置 CHG-*："
                        + "、".join(sorted(config_changes)),
                    )
                if not evidence_refs or not evidence_refs.issubset(valid_evidence):
                    self.error(
                        "E_FINAL_CONFIG_EVIDENCE",
                        config_check.line,
                        "配置修改检查必须引用当前仍适用的 EV-*",
                    )
                for evidence_id in evidence_refs & valid_evidence:
                    evidence = self.evidence[evidence_id]
                    supported = evidence.fields.get("支持或否定")
                    final_refs = (
                        set(find_refs(FINAL_ID_RE, supported.value))
                        if supported is not None
                        else set()
                    )
                    if "FINAL-CONFIG" not in final_refs:
                        self.error(
                            "E_FINAL_EVIDENCE_BACKLINK",
                            supported.line if supported is not None else evidence.line,
                            f"证据 {evidence_id} 的“支持或否定”必须列出 FINAL-CONFIG",
                            "FINAL-CONFIG",
                        )

        risks = final_fields.get("范围外风险")
        if risks is not None and is_blank(risks.value):
            self.error(
                "E_FINAL_RISKS",
                risks.line,
                "最终完成时必须明确填写范围外风险；没有则写“无”",
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


def split_path_and_identity(value: str) -> tuple[Path | None, str | None]:
    parts = re.split(r"[；;]", value, maxsplit=1)
    if len(parts) != 2:
        return None, None
    path_text, identity = (part.strip() for part in parts)
    if not path_text or is_blank(identity):
        return None, None
    return Path(path_text), identity


def markdown_path_references(value: str) -> list[tuple[str, str | None, bool]]:
    return [
        (
            match.group("path").strip(),
            match.group("anchor"),
            match.group("whole") is not None,
        )
        for match in MARKDOWN_PATH_RE.finditer(value)
    ]


def markdown_heading_anchors(source: str) -> set[str]:
    anchors: set[str] = set()
    occurrences: dict[str, int] = defaultdict(int)
    fence_marker: str | None = None
    for line in source.splitlines():
        stripped = line.lstrip()
        opening_fence = re.match(r"^(`{3,}|~{3,})", stripped)
        if fence_marker is None and opening_fence is not None:
            fence_marker = opening_fence.group(1)
            continue
        if fence_marker is not None:
            if stripped.startswith(fence_marker):
                fence_marker = None
            continue
        match = re.match(r"^(#{1,6})\s+(.+?)\s*$", stripped)
        if match is None:
            continue
        heading = match.group(2).rstrip("#").rstrip()
        base = markdown_heading_slug(heading)
        if not base:
            continue
        occurrence = occurrences[base]
        occurrences[base] += 1
        anchors.add(base if occurrence == 0 else f"{base}-{occurrence}")
    return anchors


def markdown_heading_slug(heading: str) -> str:
    output: list[str] = []
    for character in heading:
        if character.isalnum() or character in {"-", "_"}:
            output.append(character.lower())
        elif character.isspace():
            output.append("-")
    return "".join(output)


def path_is_within(path: Path, root: Path) -> bool:
    try:
        _ = path.relative_to(root)
    except ValueError:
        return False
    return True


def change_number(identifier: str) -> int:
    return int(identifier.split("-", 1)[1])


def parse_arguments(argv: Sequence[str]) -> Arguments:
    parser = argparse.ArgumentParser(
        description="只读检查 ATT 翻译任务清单的结构、文件引用和相互引用。"
    )
    _ = parser.add_argument("task_list", type=Path, help="任务清单的绝对路径")
    _ = parser.add_argument("--final", action="store_true", help="额外检查最终完成状态")
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
        task_list=cast(Path, namespace.task_list),
        final=cast(bool, namespace.final),
        output_format=cast(str, namespace.output_format),
        warnings_as_errors=cast(bool, namespace.warnings_as_errors),
    )


def read_task_list(path: Path) -> str:
    if not path.is_absolute():
        raise ValueError("任务清单路径必须是绝对路径")
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
        text = read_task_list(arguments.task_list)
    except (OSError, UnicodeError, ValueError) as error:
        emit_invocation_error(arguments.task_list, str(error), arguments.output_format)
        return 2

    diagnostics = TaskListValidator(
        path=arguments.task_list,
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
