#!/usr/bin/env python3
"""把最终 Extract 的 Manual TOML 整理为 Formic 自然来源单元。"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    atomic_write_directory,
    display_path,
    protect_outputs,
    read_manual,
    run_cli,
)
from term_toolbox.grouping import build_formic_units, render_formic_unit

_INVALID_FILENAME = re.compile(r'[<>:"/\\|?*\x00-\x1f]')
_TASK = """\
从当前剧情分片直接找出可能在翻译时写法不一致的游戏专有单个名词，只输出原文，一行一个。
只摘录游戏文本中真实出现的内容。不要造词，不要拼接多个名词，不要输出短语、句子、普通词或译文。
不去重、不统计次数、不判断最终是否收录；看到候选就列出。需要理解候选时，可以搜索或读取 input 中的其他文本。
没有候选时输出“无”。
"""


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="一次建立 Formic input、plan.jsonl 和 task.md。")
    parser.add_argument(
        "--manual", type=Path, required=True, help="首次 Translate 前的最终 Manual export TOML"
    )
    parser.add_argument("--output", type=Path, required=True, help="Formic 作业目录")
    parser.add_argument("--replace", action="store_true")
    return parser


def _safe_name(value: str) -> str:
    cleaned = _INVALID_FILENAME.sub("_", value).rstrip(" .")
    return cleaned or "未分类来源"


def _prepare(args: argparse.Namespace) -> int:
    entries = read_manual(args.manual)
    protect_outputs([args.output], inputs=[args.manual], replace=args.replace)
    units = build_formic_units(entries)
    files: dict[str, str] = {"task.md": _TASK}
    plan_lines: list[str] = []
    for unit_number, unit in enumerate(units, start=1):
        base = _safe_name(unit.title)[:80].rstrip(" .") or "未分类来源"
        name = f"{unit_number:06d}-{base}.md"
        relative = f"input/{name}"
        files[relative] = render_formic_unit(unit)
        plan: dict[str, JsonValue] = {"unit": unit_number, "files": [name]}
        plan_lines.append(json.dumps(plan, ensure_ascii=False, separators=(",", ":")))
    files["plan.jsonl"] = "\n".join(plan_lines) + ("\n" if plan_lines else "")
    atomic_write_directory(args.output, files, replace=args.replace)
    maximum_characters = max((unit.source_characters for unit in units), default=0)
    maximum_rendered = max((len(render_formic_unit(unit)) for unit in units), default=0)
    print(
        f"已把 {len(entries)} 个当前可翻译条目按公开自然范围建立 {len(units)} 个 Formic 单元；"
        f"最大单元原文 {maximum_characters} 字符、完整 Markdown {maximum_rendered} 字符："
        f"{display_path(args.output)}"
    )
    print("下一步：在 Formic 目录使用 output/input、output/plan.jsonl 和 output/task.md 运行全部单元。")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _prepare(parsed))
