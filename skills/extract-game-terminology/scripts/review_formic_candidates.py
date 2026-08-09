#!/usr/bin/env python3
"""用完整 Manual 语料核对 Formic 候选的真实出现次数与位置。"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    display_path,
    fail,
    protect_outputs,
    read_manual,
    require_directory,
    run_cli,
    safe_walk_files,
    scan_term_occurrences,
    write_json,
)
from term_toolbox.grouping import build_formic_units

_BULLET = re.compile(r"^(?:[-*+]\s+|[0-9]+[.)]\s+)")


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="只读 Formic 数字 Markdown，删除不存在、单次和重复候选。")
    parser.add_argument("--manual", type=Path, required=True, help="与 Formic 作业一致的完整 Manual TOML")
    parser.add_argument("--formic-out", type=Path, required=True, help="Formic out 根目录")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replace", action="store_true")
    return parser


def _candidate_lines(root: Path) -> tuple[list[str], list[str]]:
    resolved_root = root.resolve(strict=True)
    files = sorted(
        (
            path
            for path in safe_walk_files(resolved_root)
            if path.parent == resolved_root and path.suffix.lower() == ".md" and path.stem.isdecimal()
        ),
        key=lambda path: int(path.stem),
    )
    candidates: list[str] = []
    for path in files:
        for raw_line in path.read_text(encoding="utf-8-sig").splitlines():
            line = _BULLET.sub("", raw_line.strip()).strip("` ")
            if not line or line == "无" or line.startswith("#"):
                continue
            candidates.append(line)
    return candidates, [path.name for path in files]


def _review(args: argparse.Namespace) -> int:
    entries = read_manual(args.manual)
    output_root = require_directory(args.formic_out, "Formic out 目录")
    protect_outputs([args.output], inputs=[args.manual, output_root], replace=args.replace)
    raw_candidates, files = _candidate_lines(output_root)
    expected_units = len(build_formic_units(entries))
    observed_units = [int(Path(name).stem) for name in files]
    if observed_units != list(range(1, expected_units + 1)):
        fail(
            str(output_root),
            f"Formic 数字结果不完整：期望 1..{expected_units}，实际 {observed_units}",
            "完成 prepare_formic_job.py 生成的全部 plan 单元后再审核候选",
        )
    accepted: list[JsonValue] = []
    rejected: list[JsonValue] = []
    seen: set[str] = set()
    unique_candidates: list[str] = []
    for candidate in raw_candidates:
        if candidate in seen:
            rejected.append({"term": candidate, "reason": "duplicate_worker_candidate"})
            continue
        seen.add(candidate)
        unique_candidates.append(candidate)
    occurrences = scan_term_occurrences(unique_candidates, entries)
    for candidate in unique_candidates:
        occurrence = occurrences[candidate]
        locations: list[JsonValue] = [
            {"id": readable_id, "occurrences": count} for readable_id, count in occurrence.locations
        ]
        if occurrence.count == 0:
            rejected.append({"term": candidate, "reason": "not_found_in_corpus"})
        elif occurrence.count == 1:
            rejected.append({"term": candidate, "reason": "single_occurrence", "locations": locations})
        else:
            accepted.append({"term": candidate, "occurrences": occurrence.count, "locations": locations})
    result: dict[str, JsonValue] = {
        "manual": str(args.manual.resolve()),
        "formic_output_files": files,
        "worker_candidate_lines": len(raw_candidates),
        "candidates": accepted,
        "rejected": rejected,
        "agent_review_required": [
            "确认候选是游戏专有的单个名词，而不是多名词组合、短语、句子或普通词。",
            "删除已有稳定固定译法、不需要全局约束的候选。",
            "确定完全汉化且在全游戏不冲突的译名。",
        ],
    }
    write_json(args.output, result, replace=args.replace)
    print(
        f"已读取 {len(files)} 份 Formic 数字结果，候选行 {len(raw_candidates)} 条；"
        f"核对后保留 {len(accepted)} 个多次出现候选。"
    )
    print(f"待 Agent 统一审核：{display_path(args.output)}")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _review(parsed))
