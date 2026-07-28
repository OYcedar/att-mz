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
    / "validate_ledger.py"
)
TEMPLATE = SCRIPT.parents[1] / "assets" / "task-ledger-template.md"
SPEC = importlib.util.spec_from_file_location("validate_ledger", SCRIPT)
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


def todo(prefix: str, number: int, state: str = "TODO") -> str:
    checked = "x" if state in {"DONE", "N/A", "SUPERSEDED", "CANCELLED"} else " "
    evidence = "EV-001" if state == "DONE" else "—"
    extra = ""
    if state == "N/A":
        extra = "\n  - 适用性核实：不适用（EV-001）"
    return f"""- [{checked}] `{prefix}-{number:03d}` `[{state}]` 测试责任
  - 父项：—
  - 依赖：—
  - 完成条件：取得规定结果
  - 执行者：主 Agent
  - 最近结果：{"Succeeded" if state in {"DONE", "N/A"} else "NotRun"}
  - 结果与证据：{evidence}
  - 剩余与恢复入口：{prefix}-{number:03d}{extra}
"""


def ledger_text(root: Path, path: Path, final: bool = False) -> str:
    phase_rows = []
    phase_sections = []
    for index, (name, prefix) in enumerate(PHASES, start=1):
        phase_state = "DONE" if final else "TODO"
        phase_rows.append(
            f"| {name} | {phase_state} | {prefix}-002 | {prefix}-003 | — | {prefix}-002 |"
        )
        todo_state = "DONE" if final else "TODO"
        phase_sections.append(
            f"## {index}. {name}（{prefix}）\n\n"
            + todo(prefix, 1, todo_state)
            + "\n"
            + todo(prefix, 2, todo_state)
            + "\n"
            + todo(prefix, 3, todo_state)
            + ("\n" + todo(prefix, 4, todo_state) if prefix == "TRN" else "")
        )

    completion = "完成" if final else "未完成"
    overall = "DONE" if final else "DOING"
    final_ref = "EV-001" if final else "待完成"
    return f"""# ATT 翻译任务：测试

## 整体方案

完成五阶段并验证最终交付。

## 任务契约

- 任务 ID：{path.stem}
- 总体状态：{overall}
- 创建时间：2026-07-28T23:00:00+08:00
- 最后更新时间：2026-07-28T23:01:00+08:00
- 当前唯一写入者：主 Agent
- 任务根：{root}
- 任务清单：{path}
- 游戏根：D:\\game
- 引擎与 ATT 项目：mz；test
- ATT 项目工作区：D:\\project
- 翻译资源根：D:\\translation
- 候选与最终输出：D:\\output
- 用户目标：交付翻译
- 成功条件：五阶段完成
- 必须保持的现有行为：游戏可运行
- 范围与停止线：最终制品
- 已授权副作用：测试目录
- 未授权或需用户选择：无

## 项目全局事实与影响分析

已完成只读勘察。

## 阶段总览与当前恢复入口

| 阶段 | 当前状态 | 最新进入门 | 最新验收门 | 使结论失效的变更 | 恢复入口 |
|---|---|---|---|---|---|
{chr(10).join(phase_rows)}

- 当前恢复入口：UNP-002

{chr(10).join(phase_sections)}

## 证据登记

### EV-001

- 类型与观察时间：文件；2026-07-28T23:00:00+08:00
- 来源定位：D:\\evidence
- 直接观察：测试事实
- 支持或否定：UNP-001、最终完成声明
- 适用范围：测试
- 当前有效性：有效

## 方案与变更记录

暂无变更。

## 阻塞、剩余风险、恢复与移交

- 当前阻塞：无
- 安全恢复入口：UNP-002

## 最终完成判断

- 完成声明：{completion}
- 用户成功条件核对：{final_ref}
- 最终产物证据：{final_ref}
- 必须保持行为证据：{final_ref}
- 配置变更验收：无配置变更
- 剩余风险：无
"""


class LedgerValidatorTests(unittest.TestCase):
    def make_ledger(
        self, final: bool = False
    ) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        target = root / "att-tasks" / "20260728-230000-mz-test.md"
        target.parent.mkdir()
        target.write_text(ledger_text(root, target, final=final), encoding="utf-8")
        return temporary, target

    def test_valid_in_progress_has_only_open_gate_warnings(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        diagnostics = MODULE.LedgerValidator(
            target, target.read_text(encoding="utf-8"), final=False
        ).validate()
        self.assertFalse([item for item in diagnostics if item.level == "ERROR"])
        self.assertEqual(
            10,
            len([item for item in diagnostics if item.code == "W_OPEN_GATE"]),
        )

    def test_valid_final_passes_without_diagnostics(self) -> None:
        temporary, target = self.make_ledger(final=True)
        self.addCleanup(temporary.cleanup)
        diagnostics = MODULE.LedgerValidator(
            target, target.read_text(encoding="utf-8"), final=True
        ).validate()
        self.assertEqual([], diagnostics)

    def test_status_checkbox_mismatch_is_rejected(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "- [ ] `TRN-001` `[TODO]`", "- [x] `TRN-001` `[TODO]`"
        )
        diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
        self.assertIn("E_STATUS_CHECKBOX", {item.code for item in diagnostics})

    def test_missing_section_is_rejected(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "## 5. 封包（RPK）", "### 5. 封包（RPK）"
        )
        diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
        self.assertIn("E_SECTION_ORDER", {item.code for item in diagnostics})

    def test_missing_fixed_terminology_responsibility_is_rejected(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(todo("TRN", 4), "")
        diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
        self.assertIn("E_FIXED_TODO_MISSING", {item.code for item in diagnostics})

    def test_fixed_terminology_responsibility_cannot_be_skipped(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            todo("TRN", 4), todo("TRN", 4, "N/A")
        )
        diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
        self.assertIn("E_GATE_TERMINAL_STATE", {item.code for item in diagnostics})

    def test_code_block_example_is_ignored(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8").replace(
            "暂无变更。",
            "```markdown\n- [x] `BAD-001` `[TODO]` 错误示例\n```",
        )
        diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
        self.assertNotIn("E_TODO_SYNTAX", {item.code for item in diagnostics})

    def test_final_rejects_open_responsibility(self) -> None:
        temporary, target = self.make_ledger(final=True)
        self.addCleanup(temporary.cleanup)
        text = (
            target.read_text(encoding="utf-8")
            .replace("- [x] `WBK-003` `[DONE]`", "- [ ] `WBK-003` `[BLOCKED]`")
            .replace(
                "  - 最近结果：Succeeded\n  - 结果与证据：EV-001\n"
                "  - 剩余与恢复入口：WBK-003",
                "  - 最近结果：OutcomeUnknown\n  - 结果与证据：EV-001\n"
                "  - 剩余与恢复入口：WBK-003\n"
                "  - 阻塞原因：发布终态未知\n"
                "  - 已确认状态：已开始发布",
                1,
            )
        )
        diagnostics = MODULE.LedgerValidator(target, text, final=True).validate()
        self.assertIn("E_OPEN_TODO", {item.code for item in diagnostics})

    def test_outcome_unknown_is_valid_only_as_blocked(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        blocked = """- [ ] `WBK-003` `[BLOCKED]` 测试责任
  - 父项：—
  - 依赖：—
  - 完成条件：取得规定结果
  - 执行者：主 Agent
  - 最近结果：OutcomeUnknown
  - 结果与证据：EV-001
  - 剩余与恢复入口：WBK-003；先核清发布终态
  - 阻塞原因：发布已经开始但未观察到终态
  - 已确认状态：不得重跑同类写回
"""
        text = target.read_text(encoding="utf-8").replace(todo("WBK", 3), blocked)
        diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
        self.assertFalse([item for item in diagnostics if item.level == "ERROR"])
        self.assertIn("W_OPEN_GATE", {item.code for item in diagnostics})

    def test_valid_na_packaging_passes_final_validation(self) -> None:
        temporary, target = self.make_ledger(final=True)
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8")
        text = text.replace(todo("RPK", 1, "DONE"), todo("RPK", 1, "N/A"))
        text = text.replace(
            "| 封包 | DONE | RPK-002 | RPK-003 | — | RPK-002 |",
            "| 封包 | N/A | RPK-002 | RPK-003 | — | RPK-002 |",
        )
        diagnostics = MODULE.LedgerValidator(target, text, final=True).validate()
        self.assertEqual([], diagnostics)

    def test_config_change_with_open_tasks_warns(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8")
        extra_todos = "\n".join(todo("TRN", number) for number in (5, 6))
        text = text.replace("## 4. 写回（WBK）", f"{extra_todos}\n## 4. 写回（WBK）")
        text = text.replace(
            "- 支持或否定：UNP-001、最终完成声明",
            "- 支持或否定：UNP-001、CHG-001、最终完成声明",
        )
        text = text.replace(
            "暂无变更。",
            """### CHG-001

- 时间：2026-07-28T23:02:00+08:00
- 配置变更：是
- 触发证据：EV-001
- 原方案或判断：保留现行规则
- 新方案与理由：修改规则并验证
- 替代方案、保留行为与代价：已比较
- 受影响的 TODO、阶段与完成声明：TRN-004
- 新增、替代或重新核实 TODO：TRN-004、TRN-005、TRN-006
- 安全恢复入口：TRN-004
- 根因：规则范围错误
- 现实消费者与影响范围：全部 kind 和 Owner
- 修改 TODO：TRN-004
- 修改前验证 TODO：TRN-005
- 修改后验证 TODO：TRN-006""",
        )
        diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
        self.assertEqual(
            3,
            len([item for item in diagnostics if item.code == "W_OPEN_CONFIG_GATE"]),
        )
        self.assertFalse([item for item in diagnostics if item.level == "ERROR"])

    def test_dependency_cycle_is_rejected(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        text = target.read_text(encoding="utf-8")
        unp_002 = todo("UNP", 2).replace("  - 依赖：—", "  - 依赖：UNP-003")
        unp_003 = todo("UNP", 3).replace("  - 依赖：—", "  - 依赖：UNP-002")
        text = text.replace(todo("UNP", 2), unp_002)
        text = text.replace(todo("UNP", 3), unp_003)
        diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
        self.assertIn("E_DEPENDENCY_CYCLE", {item.code for item in diagnostics})

    def test_cli_exit_codes_and_json(self) -> None:
        temporary, target = self.make_ledger()
        self.addCleanup(temporary.cleanup)
        output = io.StringIO()
        with redirect_stdout(output):
            warning_code = MODULE.main([str(target), "--warnings-as-errors"])
        self.assertEqual(3, warning_code)

        final_temporary, final_target = self.make_ledger(final=True)
        self.addCleanup(final_temporary.cleanup)
        output = io.StringIO()
        with redirect_stdout(output):
            success_code = MODULE.main(
                [str(final_target), "--final", "--format", "json"]
            )
        self.assertEqual(0, success_code)
        self.assertEqual([], json.loads(output.getvalue()))

        missing = target.parent / "missing.md"
        output = io.StringIO()
        with redirect_stdout(output):
            input_code = MODULE.main([str(missing)])
        self.assertEqual(2, input_code)

    def test_distributed_template_matches_validator_contract(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "att-tasks" / "20260728-230000-mz-template.md"
            target.parent.mkdir()
            text = TEMPLATE.read_text(encoding="utf-8")
            replacements = {
                "<项目名>": "template",
                "<与文件 stem 一致>": target.stem,
                "<本文件绝对路径>": str(target),
                "<绝对路径>": str(root),
            }
            for old, new in replacements.items():
                text = text.replace(old, new)
            text = re.sub(r"<[^>\n]+>", "已核实", text)
            diagnostics = MODULE.LedgerValidator(target, text, final=False).validate()
            self.assertFalse(
                [item for item in diagnostics if item.level == "ERROR"],
                diagnostics,
            )


if __name__ == "__main__":
    unittest.main()
