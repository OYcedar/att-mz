#!/usr/bin/env python3
"""建立、核对并完成 ATT 游戏术语作业。"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Callable
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import ToolArgumentParser, run_cli
from term_toolbox.finalize import configure_parser as configure_finalize
from term_toolbox.finalize import finalize
from term_toolbox.prepare import configure_parser as configure_prepare
from term_toolbox.prepare import prepare
from term_toolbox.review import configure_parser as configure_review
from term_toolbox.review import review


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="建立 Formic 术语作业、核对结果并写出 ATT 术语 TOML。")
    subparsers = parser.add_subparsers(dest="command", required=True)
    prepare_parser = subparsers.add_parser("prepare", help="从最终 Manual 建立 Formic 作业")
    configure_prepare(prepare_parser)
    review_parser = subparsers.add_parser("review", help="核对 Formic 完成结果和候选")
    configure_review(review_parser)
    finalize_parser = subparsers.add_parser("finalize", help="把 Agent 审核结果写成 ATT 术语 TOML")
    configure_finalize(finalize_parser)
    return parser


def _run(args: argparse.Namespace) -> int:
    commands: dict[str, Callable[[argparse.Namespace], int]] = {
        "prepare": prepare,
        "review": review,
        "finalize": finalize,
    }
    return commands[args.command](args)


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _run(parsed))
