"""把最终 Extract 的 Manual TOML 整理为 Formic 自然来源单元。"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from att_skill_tools import (
    JsonValue,
    atomic_write_directory,
    display_path,
    protect_outputs,
    read_manual,
)

from .grouping import (
    FORMIC_TARGET_RENDERED_CHARACTERS,
    build_formic_units,
    formic_packing_evidence,
    render_formic_scope,
    render_formic_unit,
)

_TASK = """\
从当前剧情分片直接找出可能在翻译时写法不一致的游戏专有单个名词，只输出原文，一行一个。
只摘录游戏文本中真实出现的内容。不要造词，不要拼接多个名词，不要输出短语、句子、普通词或译文。
不去重、不统计次数、不判断最终是否收录；看到候选就列出。需要理解候选时，可以搜索或读取 input 中的其他文本。
没有候选时输出“无”。
"""


def configure_parser(parser: argparse.ArgumentParser) -> None:
    """添加 prepare 子命令参数。"""

    parser.add_argument(
        "--manual", type=Path, required=True, help="首次 Translate 前的最终 Manual export TOML"
    )
    parser.add_argument("--output", type=Path, required=True, help="Formic 作业目录")
    parser.add_argument("--replace", action="store_true")


def prepare(args: argparse.Namespace) -> int:
    entries = read_manual(args.manual)
    protect_outputs([args.output], inputs=[args.manual], replace=args.replace)
    target_characters = FORMIC_TARGET_RENDERED_CHARACTERS
    units = build_formic_units(entries, target_characters=target_characters)
    files: dict[str, str] = {"task.md": _TASK}
    plan_lines: list[str] = []
    for unit_number, unit in enumerate(units, start=1):
        scope_names: list[JsonValue] = []
        for scope in unit.scopes:
            relative = f"input/{scope.file_name}"
            files[relative] = render_formic_scope(scope)
            scope_names.append(scope.file_name)
        plan: dict[str, JsonValue] = {"unit": unit_number, "files": scope_names}
        plan_lines.append(json.dumps(plan, ensure_ascii=False, separators=(",", ":")))
    files["plan.jsonl"] = "\n".join(plan_lines) + ("\n" if plan_lines else "")
    maximum_characters = max((unit.source_characters for unit in units), default=0)
    maximum_rendered = max((len(render_formic_unit(unit)) for unit in units), default=0)
    evidence = formic_packing_evidence(units, target_characters=target_characters)
    files["packing-evidence.json"] = json.dumps(evidence, ensure_ascii=False, indent=2) + "\n"
    atomic_write_directory(args.output, files, replace=args.replace)
    print(
        f"已把 {len(entries)} 个当前可翻译条目保留为 {evidence['scopes']} 个自然 Scope 文件，"
        f"并建立 {len(units)} 个 Formic 单元；"
        f"最大单元原文 {maximum_characters} 字符、完整 Markdown {maximum_rendered} 字符："
        f"{display_path(args.output)}"
    )
    if len(units) > 250:
        print(
            "装箱边界证据："
            f"来源连续段 {evidence['source_runs']}，超大 Scope {evidence['oversized_scopes']}，"
            f"实际分片总字符 {evidence['total_rendered_characters']}，"
            f"目标 {evidence['target_rendered_characters']}。"
        )
    print("下一步：在 Formic 目录使用 output/input、output/plan.jsonl 和 output/task.md 运行全部单元。")
    return 0
