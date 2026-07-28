from __future__ import annotations

import importlib.util
import io
import json
import re
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.dont_write_bytecode = True

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / "skills"
    / "translate-rpg-maker-with-att"
    / "scripts"
    / "validate_task_list.py"
)
TEMPLATE = SCRIPT.parents[1] / "assets" / "task-list-template.md"
DOCS_ROOT = SCRIPT.parents[3] / "docs"
README = DOCS_ROOT / "README.md"
TERMINOLOGY = DOCS_ROOT / "rpg-maker" / "terminology.md"
SPEC = importlib.util.spec_from_file_location("validate_task_list", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)

PHASES = (
    ("解包", "UNP"),
    ("提取", "EXT"),
    ("翻译", "TRN"),
    ("写回", "WBK"),
    ("封包", "RPK"),
)
FIXED = {
    "UNP-001": ("阶段任务", "—", ("UNP-002", "UNP-003")),
    "UNP-002": ("阶段开始前检查", "UNP-001", ()),
    "UNP-003": ("阶段完成检查", "UNP-001", ("UNP-002",)),
    "EXT-001": ("阶段任务", "—", ("EXT-002", "EXT-003")),
    "EXT-002": ("阶段开始前检查", "EXT-001", ("UNP-003",)),
    "EXT-003": ("阶段完成检查", "EXT-001", ("EXT-002",)),
    "TRN-001": ("阶段任务", "—", ("TRN-002", "TRN-003")),
    "TRN-002": ("阶段开始前检查", "TRN-001", ("EXT-003",)),
    "TRN-003": (
        "阶段完成检查",
        "TRN-001",
        ("TRN-002", "TRN-004"),
    ),
    "TRN-004": ("术语检查", "TRN-001", ("TRN-002",)),
    "WBK-001": ("阶段任务", "—", ("WBK-002", "WBK-003")),
    "WBK-002": ("阶段开始前检查", "WBK-001", ("TRN-003",)),
    "WBK-003": ("阶段完成检查", "WBK-001", ("WBK-002",)),
    "RPK-001": ("阶段任务", "—", ("RPK-002", "RPK-003")),
    "RPK-002": ("阶段开始前检查", "RPK-001", ("WBK-003",)),
    "RPK-003": ("阶段完成检查", "RPK-001", ("RPK-002",)),
}
FIXED_IDS = tuple(FIXED)
ALL_FINAL_IDS = (
    "FINAL-GOAL",
    "FINAL-OUTPUT",
    "FINAL-PRESERVE",
)


def reference_for(identifier: str) -> str:
    path = TERMINOLOGY if identifier == "TRN-004" else README
    return f"{path}（全文）"


def todo(
    identifier: str,
    state: str = "TODO",
    *,
    task_type: str | None = None,
    parent: str | None = None,
    dependencies: tuple[str, ...] | None = None,
    evidence: str | None = None,
    owner_change: str | None = None,
    next_todo: str | None = None,
) -> str:
    if identifier in FIXED:
        fixed_type, fixed_parent, fixed_dependencies = FIXED[identifier]
        task_type = task_type or fixed_type
        parent = fixed_parent if parent is None else parent
        dependencies = fixed_dependencies if dependencies is None else dependencies
    else:
        task_type = task_type or "执行"
        parent = parent or f"{identifier[:3]}-001"
        dependencies = dependencies or ()

    checked = "x" if state in {"DONE", "N/A", "SUPERSEDED", "CANCELLED"} else " "
    if evidence is None:
        evidence = "EV-001" if state in {"DONE", "N/A", "CANCELLED"} else "—"
    result = {
        "DONE": "Succeeded",
        "N/A": "Succeeded",
        "CANCELLED": "Cancelled",
    }.get(state, "NotRun")
    dependency_text = "、".join(dependencies) if dependencies else "—"
    lines = [
        f"- [{checked}] `{identifier}` `[{state}]` 测试任务",
        f"  - 任务类型：{task_type}",
        f"  - 上级 TODO：{parent}",
        f"  - 需要先完成：{dependency_text}",
        "  - 完成标准：得到可以检查的结果",
    ]
    if task_type == "阶段开始前检查" or identifier == "TRN-004":
        lines.append(f"  - 参考文档：{reference_for(identifier)}")
    lines.extend(
        (
            "  - 负责人：任务负责人",
            f"  - 最近执行结果：{result}",
            f"  - 结果和证据：{evidence}",
            f"  - 下一步：{next_todo or identifier}",
        )
    )
    if owner_change is not None:
        lines.append(f"  - 所属变更：{owner_change}")
    if state == "N/A":
        lines.append(f"  - 不适用依据：不适用（{evidence}）")
    if state == "BLOCKED":
        lines.extend(("  - 阻塞原因：等待外部事实", "  - 已确认情况：已有输入已保存"))
    if state == "SUPERSEDED":
        lines.append("  - 替代 TODO：UNP-004")
    if state == "CANCELLED":
        lines.append(f"  - 取消依据：用户决定（{evidence}）")
    return "\n".join(lines)


def evidence_record(
    identifier: str,
    support: tuple[str, ...],
    *,
    applicability: str = "是",
) -> str:
    return f"""### {identifier}

- 记录时间和类型：2026-07-28T23:00:00+08:00；文件
- 来源：D:\\evidence\\{identifier}.txt
- 观察结果：测试事实
- 支持或否定：{"、".join(support)}
- 适用范围：测试
- 当前是否适用：{applicability}"""


def task_list_text(root: Path, path: Path, *, final: bool = False) -> str:
    phase_rows: list[str] = []
    sections: list[str] = []
    state = "DONE" if final else "TODO"
    for index, (name, prefix) in enumerate(PHASES, start=1):
        next_todo = f"{prefix}-003" if final else f"{prefix}-002"
        terminology = "TRN-004" if prefix == "TRN" else "—"
        phase_rows.append(
            f"| {name} | {state} | {prefix}-001 | {prefix}-002 | "
            f"{prefix}-003 | {terminology} | — | {next_todo} |"
        )
        identifiers = [f"{prefix}-{number:03d}" for number in range(1, 4)]
        if prefix == "TRN":
            identifiers.append("TRN-004")
        sections.append(
            f"## {index}. {name}（{prefix}）\n\n"
            + "\n\n".join(todo(identifier, state) for identifier in identifiers)
        )

    overall = "DONE" if final else "DOING"
    completion = "完成" if final else "未完成"
    final_value = "EV-001" if final else "待完成"
    global_next = "RPK-003" if final else "UNP-002"
    support = (*FIXED_IDS, *ALL_FINAL_IDS)
    return f"""# ATT 翻译任务：测试

## 整体方案

完成五个阶段并检查最终文件。

## 任务信息

- 任务 ID：{path.stem}
- 总体状态：{overall}
- 创建时间：2026-07-28T23:00:00+08:00
- 最后更新时间：2026-07-28T23:01:00+08:00
- 当前唯一写入者：任务负责人
- 任务目录：{root}
- 任务清单：{path}
- 游戏来源：D:\\game
- 引擎与 ATT 项目：mz；test
- ATT 项目目录：D:\\project
- 实际使用的 ATT：{Path(sys.executable)}；测试 Python 程序
- ATT 文档版本：{DOCS_ROOT}；测试文档
- 翻译资源目录：D:\\translation
- 候选与最终文件：D:\\output
- 用户目标：交付翻译
- 成功标准：五个阶段完成
- 必须保持的行为：游戏可以运行
- 任务范围：生成最终文件
- 已授权操作：测试目录
- 需要用户决定：无

## 项目概况与影响范围

已完成只读调查。

## 阶段总览与下一步

| 阶段 | 当前状态 | 当前阶段任务 | 当前开始前检查 | 当前完成检查 | 当前术语检查 | 待重新验证的变更 | 下一步 |
|---|---|---|---|---|---|---|---|
{chr(10).join(phase_rows)}

- 全局下一步：{global_next}
- 最近确认的安全状态：测试状态

{chr(10).join(sections)}

## 证据

{evidence_record("EV-001", support)}

## 计划和变更记录

暂无变更。

## 阻塞、剩余工作和交接

- 当前阻塞：无
- 剩余 TODO：{"无" if final else "全部固定 TODO"}
- 范围外风险：无
- 下一步：{global_next}
- 交接说明：读取任务清单后继续

## 最终检查

- 完成状态：{completion}
- 用户目标：{final_value}
- 最终文件：{final_value}
- 必须保持的行为：{final_value}
- 配置修改检查：{"无配置修改" if final else "待完成"}
- 范围外风险：无
"""


def change_record(*, status: str = "待处理", config: bool = False) -> str:
    config_fields = ""
    if config:
        config_fields = f"""
- 参考文档：{README}（全文）
- 原因：规则范围错误
- 使用该配置的位置和影响范围：全部相关文本
- 修改前检查 TODO：UNP-007
- 修改 TODO：UNP-008
- 修改后检查 TODO：UNP-009"""
    rechecks = ["UNP-004", "UNP-005", "UNP-006"]
    if config:
        rechecks.extend(("UNP-007", "UNP-008", "UNP-009"))
    next_todo = "UNP-006" if status == "已完成" else "UNP-005"
    return f"""### CHG-001

- 时间：2026-07-28T23:02:00+08:00
- 处理状态：{status}
- 是否修改项目配置：{"是" if config else "否"}
- 触发证据：EV-003
- 原计划或判断：使用旧结果
- 新计划和原因：根据新证据重新检查
- 其他方案及影响：已比较
- 受影响范围：UNP-001、UNP-002、UNP-003；解包阶段
- 重新检查 TODO：{"、".join(rechecks)}
- 下一步：{next_todo}{config_fields}"""


def change_record_for(
    identifier: str,
    *,
    trigger_evidence: str,
    affected: tuple[str, ...],
    rechecks: tuple[str, ...],
    next_todo: str,
    config_tasks: tuple[str, str, str] | None = None,
) -> str:
    config_fields = ""
    if config_tasks is not None:
        before, modify, after = config_tasks
        config_fields = f"""
- 参考文档：{README}（全文）
- 原因：规则范围错误
- 使用该配置的位置和影响范围：全部相关文本
- 修改前检查 TODO：{before}
- 修改 TODO：{modify}
- 修改后检查 TODO：{after}"""
    return f"""### {identifier}

- 时间：2026-07-28T23:03:00+08:00
- 处理状态：已完成
- 是否修改项目配置：{"是" if config_tasks is not None else "否"}
- 触发证据：{trigger_evidence}
- 原计划或判断：使用旧结果
- 新计划和原因：根据新证据重新检查
- 其他方案及影响：已比较
- 受影响范围：{"、".join(affected)}
- 重新检查 TODO：{"、".join(rechecks)}
- 下一步：{next_todo}{config_fields}"""


def append_evidence(text: str, *records: str) -> str:
    marker = "\n## 计划和变更记录"
    joined_records = "\n\n".join(records)
    return text.replace(marker, f"\n\n{joined_records}{marker}", 1)


def append_change(text: str, record: str) -> str:
    marker = "\n## 阻塞、剩余工作和交接"
    return text.replace(marker, f"\n\n{record}{marker}", 1)


def add_unp_change(text: str, *, completed: bool, config: bool = False) -> str:
    state = "DONE" if completed else "TODO"
    evidence = "EV-004" if completed else "—"
    dynamic = [
        todo(
            "UNP-004",
            state,
            task_type="阶段任务",
            parent="—",
            dependencies=("UNP-005", "UNP-006"),
            evidence=evidence,
            owner_change="CHG-001",
            next_todo="UNP-006" if completed else "UNP-005",
        ),
        todo(
            "UNP-005",
            state,
            task_type="阶段开始前检查",
            parent="UNP-004",
            evidence=evidence,
            owner_change="CHG-001",
        ),
        todo(
            "UNP-006",
            state,
            task_type="阶段完成检查",
            parent="UNP-004",
            dependencies=("UNP-005",),
            evidence=evidence,
            owner_change="CHG-001",
        ),
    ]
    if config:
        dynamic.extend(
            (
                todo(
                    "UNP-007",
                    state,
                    task_type="调查",
                    evidence=evidence,
                    owner_change="CHG-001",
                ),
                todo(
                    "UNP-008",
                    state,
                    dependencies=("UNP-007",),
                    evidence=evidence,
                    owner_change="CHG-001",
                ),
                todo(
                    "UNP-009",
                    state,
                    task_type="调查",
                    dependencies=("UNP-008",),
                    evidence=evidence,
                    owner_change="CHG-001",
                ),
            )
        )
        dynamic[2] = todo(
            "UNP-006",
            state,
            task_type="阶段完成检查",
            parent="UNP-004",
            dependencies=("UNP-005", "UNP-007", "UNP-008", "UNP-009"),
            evidence=evidence,
            owner_change="CHG-001",
        )
    text = text.replace(
        "## 2. 提取（EXT）", "\n\n".join(dynamic) + "\n\n## 2. 提取（EXT）"
    )

    for identifier in ("UNP-001", "UNP-002", "UNP-003"):
        text = text.replace(
            todo(identifier, "DONE"),
            todo(identifier, "DONE", evidence="EV-002"),
        )
    text = text.replace(
        evidence_record("EV-001", (*FIXED_IDS, *ALL_FINAL_IDS)),
        "\n\n".join(
            (
                evidence_record(
                    "EV-001",
                    tuple(
                        identifier
                        for identifier in (*FIXED_IDS, *ALL_FINAL_IDS)
                        if not identifier.startswith("UNP-")
                    ),
                ),
                evidence_record(
                    "EV-002",
                    ("UNP-001", "UNP-002", "UNP-003"),
                    applicability="否（CHG-001）",
                ),
                evidence_record("EV-003", ("CHG-001",)),
                evidence_record(
                    "EV-004",
                    tuple(
                        [
                            "UNP-004",
                            "UNP-005",
                            "UNP-006",
                            *(("UNP-007", "UNP-008", "UNP-009") if config else ()),
                            *(("FINAL-CONFIG",) if config else ()),
                        ]
                    ),
                ),
            )
        ),
    )
    text = text.replace(
        "暂无变更。",
        change_record(status="已完成" if completed else "待处理", config=config),
    )

    old_row = "| 解包 | DONE | UNP-001 | UNP-002 | UNP-003 | — | — | UNP-003 |"
    new_row = (
        "| 解包 | DONE | UNP-004 | UNP-005 | UNP-006 | — | — | UNP-006 |"
        if completed
        else ("| 解包 | TODO | UNP-004 | UNP-005 | UNP-006 | — | CHG-001 | UNP-005 |")
    )
    text = text.replace(old_row, new_row)
    if completed:
        text = text.replace(
            todo("EXT-002", "DONE"),
            todo("EXT-002", "DONE", dependencies=("UNP-003", "UNP-006")),
        )
    if not completed:
        text = text.replace("- 总体状态：DONE", "- 总体状态：DOING")
        text = text.replace("- 全局下一步：RPK-003", "- 全局下一步：UNP-005")
        text = text.replace("- 完成状态：完成", "- 完成状态：未完成")
    if config and completed:
        text = text.replace(
            "- 配置修改检查：无配置修改",
            "- 配置修改检查：CHG-001、EV-004",
        )
    return text


def add_second_completed_unp_change(text: str) -> str:
    historical_work = todo(
        "UNP-007",
        "DONE",
        evidence="EV-004",
        owner_change="CHG-001",
    )
    new_todos = (
        todo(
            "UNP-008",
            "DONE",
            task_type="阶段任务",
            parent="—",
            dependencies=("UNP-009", "UNP-010"),
            evidence="EV-006",
            owner_change="CHG-002",
            next_todo="UNP-010",
        ),
        todo(
            "UNP-009",
            "DONE",
            task_type="阶段开始前检查",
            parent="UNP-008",
            evidence="EV-006",
            owner_change="CHG-002",
        ),
        todo(
            "UNP-010",
            "DONE",
            task_type="阶段完成检查",
            parent="UNP-008",
            dependencies=("UNP-009",),
            evidence="EV-006",
            owner_change="CHG-002",
        ),
    )
    text = text.replace(
        todo(
            "UNP-006",
            "DONE",
            task_type="阶段完成检查",
            parent="UNP-004",
            dependencies=("UNP-005",),
            evidence="EV-004",
            owner_change="CHG-001",
        ),
        todo(
            "UNP-006",
            "DONE",
            task_type="阶段完成检查",
            parent="UNP-004",
            dependencies=("UNP-005", "UNP-007"),
            evidence="EV-004",
            owner_change="CHG-001",
        ),
    )
    text = text.replace(
        "## 2. 提取（EXT）",
        "\n\n".join((historical_work, *new_todos)) + "\n\n## 2. 提取（EXT）",
    )
    text = text.replace(
        todo("EXT-002", "DONE", dependencies=("UNP-003", "UNP-006")),
        todo("EXT-002", "DONE", dependencies=("UNP-003", "UNP-010")),
    )
    text = text.replace(
        evidence_record("EV-004", ("UNP-004", "UNP-005", "UNP-006")),
        evidence_record(
            "EV-004",
            ("UNP-004", "UNP-005", "UNP-006", "UNP-007"),
            applicability="否（CHG-002）",
        ),
    )
    text = append_evidence(
        text,
        evidence_record("EV-005", ("CHG-002",)),
        evidence_record("EV-006", ("UNP-008", "UNP-009", "UNP-010")),
    )
    text = text.replace(
        change_record(status="已完成"),
        change_record_for(
            "CHG-001",
            trigger_evidence="EV-003",
            affected=("UNP-001", "UNP-002", "UNP-003"),
            rechecks=("UNP-004", "UNP-005", "UNP-006", "UNP-007"),
            next_todo="UNP-006",
        ),
    )
    text = append_change(
        text,
        change_record_for(
            "CHG-002",
            trigger_evidence="EV-005",
            affected=("UNP-004", "UNP-005", "UNP-006", "UNP-007"),
            rechecks=("UNP-008", "UNP-009", "UNP-010"),
            next_todo="UNP-010",
        ),
    )
    return text.replace(
        "| 解包 | DONE | UNP-004 | UNP-005 | UNP-006 | — | — | UNP-006 |",
        "| 解包 | DONE | UNP-008 | UNP-009 | UNP-010 | — | — | UNP-010 |",
    )


def add_change_reusing_fixed_unp_todos(text: str) -> str:
    for identifier in ("UNP-001", "UNP-002", "UNP-003"):
        text = text.replace(
            todo(identifier, "DONE"),
            todo(identifier, "DONE", owner_change="CHG-001"),
        )
    text = append_evidence(text, evidence_record("EV-002", ("CHG-001",)))
    return text.replace(
        "暂无变更。",
        change_record_for(
            "CHG-001",
            trigger_evidence="EV-002",
            affected=("UNP-001", "UNP-002", "UNP-003"),
            rechecks=("UNP-001", "UNP-002", "UNP-003"),
            next_todo="UNP-003",
        ),
    )


def add_trn_change_without_new_terminology_check(text: str) -> str:
    for identifier in ("TRN-001", "TRN-002", "TRN-003", "TRN-004"):
        text = text.replace(
            todo(identifier, "DONE"),
            todo(identifier, "DONE", evidence="EV-002"),
        )
    dynamic = (
        todo(
            "TRN-005",
            "DONE",
            task_type="阶段任务",
            parent="—",
            dependencies=("TRN-006", "TRN-007"),
            evidence="EV-004",
            owner_change="CHG-001",
            next_todo="TRN-007",
        ),
        todo(
            "TRN-006",
            "DONE",
            task_type="阶段开始前检查",
            parent="TRN-005",
            dependencies=("EXT-003",),
            evidence="EV-004",
            owner_change="CHG-001",
        ),
        todo(
            "TRN-007",
            "DONE",
            task_type="阶段完成检查",
            parent="TRN-005",
            dependencies=("TRN-006", "TRN-004"),
            evidence="EV-004",
            owner_change="CHG-001",
        ),
    )
    text = text.replace(
        "## 4. 写回（WBK）",
        "\n\n".join(dynamic) + "\n\n## 4. 写回（WBK）",
    )
    text = text.replace(
        todo("WBK-002", "DONE"),
        todo("WBK-002", "DONE", dependencies=("TRN-003", "TRN-007")),
    )
    text = text.replace(
        evidence_record("EV-001", (*FIXED_IDS, *ALL_FINAL_IDS)),
        "\n\n".join(
            (
                evidence_record(
                    "EV-001",
                    tuple(
                        identifier
                        for identifier in (*FIXED_IDS, *ALL_FINAL_IDS)
                        if not identifier.startswith("TRN-")
                    ),
                ),
                evidence_record(
                    "EV-002",
                    ("TRN-001", "TRN-002", "TRN-003", "TRN-004"),
                    applicability="否（CHG-001）",
                ),
                evidence_record("EV-003", ("CHG-001",)),
                evidence_record("EV-004", ("TRN-005", "TRN-006", "TRN-007")),
            )
        ),
    )
    text = text.replace(
        "暂无变更。",
        change_record_for(
            "CHG-001",
            trigger_evidence="EV-003",
            affected=("TRN-001", "TRN-002", "TRN-003", "TRN-004"),
            rechecks=("TRN-005", "TRN-006", "TRN-007"),
            next_todo="TRN-007",
        ),
    )
    return text.replace(
        "| 翻译 | DONE | TRN-001 | TRN-002 | TRN-003 | TRN-004 | — | TRN-003 |",
        "| 翻译 | DONE | TRN-005 | TRN-006 | TRN-007 | TRN-004 | — | TRN-007 |",
    )


class TaskListValidatorTests(unittest.TestCase):
    def make_task_list(
        self, *, final: bool = False
    ) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        target = root / "att-tasks" / "20260728-230000-mz-test.md"
        target.parent.mkdir()
        target.write_text(task_list_text(root, target, final=final), encoding="utf-8")
        return temporary, target

    def validate(self, target: Path, text: str, *, final: bool = False) -> set[str]:
        return {
            item.code
            for item in MODULE.TaskListValidator(target, text, final=final).validate()
            if item.level == "ERROR"
        }

    def test_valid_in_progress_task_list_passes(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        self.assertEqual(
            set(), self.validate(target, target.read_text(encoding="utf-8"))
        )

    def test_valid_final_task_list_passes(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        self.assertEqual(
            set(),
            self.validate(target, target.read_text(encoding="utf-8"), final=True),
        )

    def test_only_task_owner_can_write_task_list(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "- 当前唯一写入者：任务负责人",
            "- 当前唯一写入者：协作者",
            1,
        )
        self.assertIn("E_TASK_LIST_WRITER", self.validate(target, text))

    def test_task_owner_remains_responsible_for_delegated_todo(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "  - 负责人：任务负责人",
            "  - 负责人：协作者",
            1,
        )
        self.assertIn("E_TODO_OWNER", self.validate(target, text))

    def test_done_requires_succeeded(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "  - 最近执行结果：Succeeded",
            "  - 最近执行结果：Failed",
            1,
        )
        self.assertIn("E_COMPLETED_RESULT", self.validate(target, text, final=True))

    def test_outcome_unknown_requires_blocked(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "  - 最近执行结果：NotRun",
            "  - 最近执行结果：OutcomeUnknown",
            1,
        )
        self.assertIn("E_OUTCOME_UNKNOWN_STATE", self.validate(target, text))

    def test_fixed_todo_type_parent_and_dependencies_are_checked(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8")
        text = text.replace("  - 任务类型：阶段任务", "  - 任务类型：执行", 1)
        text = text.replace("  - 上级 TODO：UNP-001", "  - 上级 TODO：—", 1)
        text = text.replace(
            "  - 需要先完成：UNP-002、UNP-003",
            "  - 需要先完成：—",
            1,
        )
        codes = self.validate(target, text)
        self.assertTrue(
            {
                "E_FIXED_TODO_TYPE",
                "E_FIXED_TODO_PARENT",
                "E_FIXED_TODO_DEPENDENCY",
            }.issubset(codes)
        )

    def test_reference_document_must_exist_under_declared_docs_root(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            str(README),
            str(DOCS_ROOT / "missing.md"),
            1,
        )
        self.assertIn("E_REFERENCE_MISSING", self.validate(target, text))

    def test_reference_document_requires_section_or_full_text(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            f"{README}（全文）",
            str(README),
            1,
        )
        self.assertIn("E_REFERENCE_SECTION", self.validate(target, text))

    def test_reference_anchor_must_exist_in_document(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            f"{README}（全文）",
            f"{README}#definitely-missing",
            1,
        )
        self.assertIn("E_REFERENCE_SECTION", self.validate(target, text))

    def test_full_text_marker_applies_only_to_its_document(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            f"{README}（全文）",
            f"{README}；{TERMINOLOGY}（全文）",
            1,
        )
        self.assertIn("E_REFERENCE_SECTION", self.validate(target, text))

    def test_successor_start_check_also_requires_reference_document(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=True)
        text = text.replace(
            f"  - 参考文档：{reference_for('UNP-005')}\n",
            "",
            1,
        )
        self.assertIn("E_REQUIRED_FIELD", self.validate(target, text))

    def test_historical_done_can_keep_evidence_replaced_by_change(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=True)
        self.assertEqual(set(), self.validate(target, text, final=True))

    def test_later_change_can_replace_completed_change_tasks_and_work(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=True)
        text = add_second_completed_unp_change(text)
        self.assertEqual(set(), self.validate(target, text, final=True))

    def test_stale_evidence_without_affected_todo_is_rejected(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=True)
        text = text.replace(
            "受影响范围：UNP-001、UNP-002、UNP-003",
            "受影响范围：UNP-001、UNP-002",
        )
        self.assertIn(
            "E_TODO_EVIDENCE_APPLICABILITY",
            self.validate(target, text, final=True),
        )

    def test_pending_change_cannot_be_omitted_from_phase_summary(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=False)
        text = text.replace(
            "| 解包 | TODO | UNP-004 | UNP-005 | UNP-006 | — | CHG-001 | UNP-005 |",
            "| 解包 | TODO | UNP-004 | UNP-005 | UNP-006 | — | — | UNP-005 |",
        )
        self.assertIn("E_PHASE_CHANGE_LIST", self.validate(target, text))

    def test_completed_change_requires_all_recheck_todos_done(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=False)
        text = text.replace("- 处理状态：待处理", "- 处理状态：已完成")
        self.assertIn("E_CHANGE_STATUS_MISMATCH", self.validate(target, text))

    def test_phase_summary_must_point_to_latest_change_tasks(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=True)
        text = text.replace(
            "| 解包 | DONE | UNP-004 | UNP-005 | UNP-006 | — | — | UNP-006 |",
            "| 解包 | DONE | UNP-001 | UNP-002 | UNP-003 | — | — | UNP-003 |",
        )
        self.assertIn("E_PHASE_CURRENT_CHANGE", self.validate(target, text))

    def test_change_rechecks_must_use_new_todo_ids(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_change_reusing_fixed_unp_todos(target.read_text(encoding="utf-8"))
        self.assertIn(
            "E_CHANGE_RECHECK_REUSE",
            self.validate(target, text, final=True),
        )

    def test_trn_004_invalidation_requires_new_terminology_check(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_trn_change_without_new_terminology_check(
            target.read_text(encoding="utf-8")
        )
        self.assertTrue(
            {
                "E_CHANGE_TERMINOLOGY_CHECK",
                "E_PHASE_CURRENT_TERMINOLOGY_CHANGE",
            }.issubset(self.validate(target, text, final=True))
        )

    def test_done_phase_rejects_open_dynamic_todo(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "## 2. 提取（EXT）",
            todo("UNP-004") + "\n\n## 2. 提取（EXT）",
        )
        self.assertIn("E_PHASE_OPEN_TODO", self.validate(target, text))

    def test_completion_check_must_depend_on_dynamic_work(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "## 2. 提取（EXT）",
            todo("UNP-004") + "\n\n## 2. 提取（EXT）",
        )
        self.assertIn("E_COMPLETION_CHECK_COVERAGE", self.validate(target, text))

    def test_done_dynamic_todo_requires_successful_dependencies(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=False)
        text = text.replace(
            todo(
                "UNP-006",
                task_type="阶段完成检查",
                parent="UNP-004",
                dependencies=("UNP-005",),
                evidence="—",
                owner_change="CHG-001",
            ),
            todo(
                "UNP-006",
                "DONE",
                task_type="阶段完成检查",
                parent="UNP-004",
                dependencies=("UNP-005",),
                evidence="EV-004",
                owner_change="CHG-001",
            ),
        )
        self.assertIn("E_DEPENDENCY_OPEN", self.validate(target, text))

    def test_dynamic_completion_must_depend_on_current_start_check(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=True)
        text = text.replace(
            todo(
                "UNP-006",
                "DONE",
                task_type="阶段完成检查",
                parent="UNP-004",
                dependencies=("UNP-005",),
                evidence="EV-004",
                owner_change="CHG-001",
            ),
            todo(
                "UNP-006",
                "DONE",
                task_type="阶段完成检查",
                parent="UNP-004",
                dependencies=(),
                evidence="EV-004",
                owner_change="CHG-001",
            ),
        )
        self.assertIn(
            "E_COMPLETION_CHECK_START",
            self.validate(target, text, final=True),
        )

    def test_downstream_start_depends_on_current_previous_completion(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=True)
        text = text.replace(
            todo("EXT-002", "DONE", dependencies=("UNP-003", "UNP-006")),
            todo("EXT-002", "DONE"),
        )
        self.assertIn(
            "E_PHASE_START_DEPENDENCY",
            self.validate(target, text, final=True),
        )

    def test_pending_change_next_must_be_open_recheck(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=False)
        text = text.replace(
            todo(
                "UNP-004",
                task_type="阶段任务",
                parent="—",
                dependencies=("UNP-005", "UNP-006"),
                evidence="—",
                owner_change="CHG-001",
                next_todo="UNP-005",
            ),
            todo(
                "UNP-004",
                task_type="阶段任务",
                parent="—",
                dependencies=("UNP-005", "UNP-006"),
                evidence="—",
                owner_change="CHG-001",
                next_todo="UNP-006",
            ),
        )
        text = text.replace(
            todo(
                "UNP-005",
                task_type="阶段开始前检查",
                parent="UNP-004",
                evidence="—",
                owner_change="CHG-001",
            ),
            todo(
                "UNP-005",
                "DONE",
                task_type="阶段开始前检查",
                parent="UNP-004",
                evidence="EV-004",
                owner_change="CHG-001",
            ),
        )
        text = text.replace(
            "| 解包 | TODO | UNP-004 | UNP-005 | UNP-006 | — | CHG-001 | UNP-005 |",
            "| 解包 | TODO | UNP-004 | UNP-005 | UNP-006 | — | CHG-001 | UNP-006 |",
        )
        text = text.replace("- 全局下一步：UNP-005", "- 全局下一步：UNP-006")
        blocking_marker = "## 阻塞、剩余工作和交接"
        before_blocking, marker, blocking = text.partition(blocking_marker)
        blocking = blocking.replace("- 下一步：UNP-005", "- 下一步：UNP-006", 1)
        text = before_blocking + marker + blocking
        self.assertIn("E_CHANGE_NEXT", self.validate(target, text))

    def test_done_phase_next_must_be_current_completion_check(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "| 解包 | DONE | UNP-001 | UNP-002 | UNP-003 | — | — | UNP-003 |",
            "| 解包 | DONE | UNP-001 | UNP-002 | UNP-003 | — | — | UNP-002 |",
        )
        self.assertIn("E_PHASE_NEXT", self.validate(target, text))

    def test_global_next_must_match_earliest_open_phase(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "- 全局下一步：UNP-002",
            "- 全局下一步：TRN-002",
        )
        self.assertIn("E_GLOBAL_NEXT", self.validate(target, text))

    def test_change_trigger_evidence_requires_backlink(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(target.read_text(encoding="utf-8"), completed=True)
        text = text.replace("- 支持或否定：CHG-001", "- 支持或否定：FINAL-GOAL")
        self.assertIn("E_CHANGE_TRIGGER_BACKLINK", self.validate(target, text))

    def test_config_change_uses_dedicated_ordered_tasks(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(
            target.read_text(encoding="utf-8"), completed=True, config=True
        )
        self.assertEqual(set(), self.validate(target, text, final=True))
        text = text.replace("- 修改前检查 TODO：UNP-007", "- 修改前检查 TODO：UNP-001")
        codes = self.validate(target, text, final=True)
        self.assertTrue({"E_CONFIG_FIXED_TODO", "E_CONFIG_RECHECK"}.intersection(codes))

    def test_config_change_requires_dependency_order(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(
            target.read_text(encoding="utf-8"), completed=True, config=True
        )
        text = text.replace(
            todo(
                "UNP-008",
                "DONE",
                dependencies=("UNP-007",),
                evidence="EV-004",
                owner_change="CHG-001",
            ),
            todo(
                "UNP-008",
                "DONE",
                dependencies=(),
                evidence="EV-004",
                owner_change="CHG-001",
            ),
        )
        self.assertIn("E_CONFIG_TASK_ORDER", self.validate(target, text, final=True))

    def test_config_change_steps_require_done_role_specific_tasks(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(
            target.read_text(encoding="utf-8"), completed=True, config=True
        )
        replacements = (
            (
                todo(
                    "UNP-007",
                    "DONE",
                    task_type="调查",
                    evidence="EV-004",
                    owner_change="CHG-001",
                ),
                todo(
                    "UNP-007",
                    "N/A",
                    task_type="阶段任务",
                    evidence="EV-004",
                    owner_change="CHG-001",
                ),
            ),
            (
                todo(
                    "UNP-008",
                    "DONE",
                    dependencies=("UNP-007",),
                    evidence="EV-004",
                    owner_change="CHG-001",
                ),
                todo(
                    "UNP-008",
                    "N/A",
                    task_type="阶段任务",
                    dependencies=("UNP-007",),
                    evidence="EV-004",
                    owner_change="CHG-001",
                ),
            ),
            (
                todo(
                    "UNP-009",
                    "DONE",
                    task_type="调查",
                    dependencies=("UNP-008",),
                    evidence="EV-004",
                    owner_change="CHG-001",
                ),
                todo(
                    "UNP-009",
                    "N/A",
                    task_type="阶段任务",
                    dependencies=("UNP-008",),
                    evidence="EV-004",
                    owner_change="CHG-001",
                ),
            ),
        )
        for current, invalid in replacements:
            text = text.replace(current, invalid)
        self.assertTrue(
            {"E_CONFIG_TASK_TYPE", "E_CONFIG_TASK_STATE"}.issubset(
                self.validate(target, text, final=True)
            )
        )

    def test_config_tasks_cannot_be_reused_by_another_change(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(
            target.read_text(encoding="utf-8"), completed=True, config=True
        )
        text = text.replace(
            "  - 所属变更：CHG-001",
            "  - 所属变更：CHG-001、CHG-002",
        )
        text = append_evidence(text, evidence_record("EV-005", ("CHG-002",)))
        text = append_change(
            text,
            change_record_for(
                "CHG-002",
                trigger_evidence="EV-005",
                affected=("UNP-001", "UNP-002", "UNP-003"),
                rechecks=(
                    "UNP-004",
                    "UNP-005",
                    "UNP-006",
                    "UNP-007",
                    "UNP-008",
                    "UNP-009",
                ),
                next_todo="UNP-006",
                config_tasks=("UNP-007", "UNP-008", "UNP-009"),
            ),
        )
        text = text.replace(
            "- 配置修改检查：CHG-001、EV-004",
            "- 配置修改检查：CHG-001、CHG-002、EV-004",
        )
        self.assertIn(
            "E_CONFIG_TASK_REUSE",
            self.validate(target, text, final=True),
        )

    def test_final_config_check_cannot_claim_no_changes(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = add_unp_change(
            target.read_text(encoding="utf-8"), completed=True, config=True
        ).replace(
            "- 配置修改检查：CHG-001、EV-004",
            "- 配置修改检查：无配置修改",
        )
        self.assertIn("E_FINAL_CONFIG", self.validate(target, text, final=True))

    def test_final_evidence_requires_exact_backlink(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "、FINAL-OUTPUT",
            "",
        )
        self.assertIn(
            "E_FINAL_EVIDENCE_BACKLINK",
            self.validate(target, text, final=True),
        )

    def test_final_evidence_backlink_rejects_suffixed_identifier(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "FINAL-OUTPUT",
            "FINAL-OUTPUT-fake",
            1,
        )
        self.assertIn(
            "E_FINAL_EVIDENCE_BACKLINK",
            self.validate(target, text, final=True),
        )

    def test_superseded_todo_requires_change_owned_replacement(self) -> None:
        temporary, target = self.make_task_list(final=True)
        self.addCleanup(temporary.cleanup)
        superseded = todo("UNP-004", "SUPERSEDED").replace(
            "  - 替代 TODO：UNP-004",
            "  - 替代 TODO：RPK-003",
        )
        text = target.read_text(encoding="utf-8").replace(
            "## 2. 提取（EXT）",
            superseded + "\n\n## 2. 提取（EXT）",
        )
        self.assertIn(
            "E_SUPERSEDED_CHANGE",
            self.validate(target, text, final=True),
        )

    def test_open_todo_next_must_match_current_phase_next(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            todo("UNP-002"),
            todo("UNP-002", next_todo="UNP-003"),
        )
        self.assertIn("E_PHASE_TODO_NEXT", self.validate(target, text))

    def test_handoff_next_must_match_global_next(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8")
        blocking_marker = "## 阻塞、剩余工作和交接"
        before_blocking, marker, blocking = text.partition(blocking_marker)
        blocking = blocking.replace("- 下一步：UNP-002", "- 下一步：RPK-003", 1)
        text = before_blocking + marker + blocking
        self.assertIn("E_HANDOFF_NEXT", self.validate(target, text))

    def test_dependency_cycle_is_rejected(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "  - 需要先完成：—",
            "  - 需要先完成：UNP-001",
            1,
        )
        self.assertIn("E_DEPENDENCY_CYCLE", self.validate(target, text))

    def test_cli_exit_codes_and_json(self) -> None:
        temporary, target = self.make_task_list()
        self.addCleanup(temporary.cleanup)
        output = io.StringIO()
        with redirect_stdout(output):
            success = MODULE.main([str(target), "--format", "json"])
        self.assertEqual(0, success)
        self.assertEqual([], json.loads(output.getvalue()))

        missing = target.parent / "missing.md"
        with redirect_stdout(io.StringIO()):
            missing_code = MODULE.main([str(missing)])
        self.assertEqual(2, missing_code)

    def test_distributed_template_matches_validator(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "att-tasks" / "20260728-230000-mz-template.md"
            target.parent.mkdir()
            text = TEMPLATE.read_text(encoding="utf-8")
            replacements = {
                "<项目名>": "template",
                "<与不含 .md 后缀的文件名一致>": target.stem,
                "<本文件绝对路径>": str(target),
                "<绝对路径>": str(root),
                "<可执行文件绝对路径；文件哈希、对应的 Git 提交 ID 或构建版本>": (
                    f"{Path(sys.executable)}；测试程序"
                ),
                "<docs 根绝对路径；对应的 Git 提交 ID 或目录内容哈希>": (
                    f"{DOCS_ROOT}；测试文档"
                ),
            }
            for old, new in replacements.items():
                text = text.replace(old, new)
            text = re.sub(
                r"<ATT 文档版本；[^>\n]+>",
                lambda _: f"{README}（全文）",
                text,
            )
            text = re.sub(r"<[^>\n]+>", "已确认", text)
            target.write_text(text, encoding="utf-8")
            self.assertEqual(set(), self.validate(target, text))


if __name__ == "__main__":
    unittest.main()
