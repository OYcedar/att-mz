#!/usr/bin/env python3
"""按 Formic v2 完成记录核对术语候选的真实出现次数与位置。"""

from __future__ import annotations

import argparse
import re
import sys
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
    read_json_object,
    read_manual,
    require_directory,
    require_file,
    require_list,
    run_cli,
    safe_walk_files,
    scan_term_occurrences,
    validate_object_keys,
    write_json,
)

_BULLET = re.compile(r"^(?:[-*+]\s+|[0-9]+[.)]\s+)")
_RUN = re.compile(r"run-([0-9]+)\Z")
_SUMMARY_FIELDS = {
    "planned",
    "already_completed",
    "started",
    "published",
    "failed",
    "stopped",
    "not_started",
    "first_failed",
    "failed_samples",
    "first_stopped",
    "stopped_samples",
    "first_incomplete",
    "incomplete_samples",
    "failure_reasons",
    "stop_reason",
    "llm_calls",
    "llm_calls_with_provider_usage",
    "llm_calls_without_provider_usage",
}


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="只读 Formic v2 results 与最新 run summary，机械筛除无效候选。")
    parser.add_argument("--manual", type=Path, required=True, help="与 Formic 作业一致的完整 Manual TOML")
    parser.add_argument("--plan", type=Path, required=True, help="prepare_formic_job.py 生成的 plan.jsonl")
    parser.add_argument("--formic-out", type=Path, required=True, help="Formic v2 OUT 根目录")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replace", action="store_true")
    return parser


def _plan_units(path: Path) -> list[int]:
    source = require_file(path, "Formic plan.jsonl")
    units: list[int] = []
    seen: set[int] = set()
    for line_number, text in enumerate(source.read_text(encoding="utf-8-sig").splitlines(), start=1):
        if not text.strip():
            fail(str(source), f"第 {line_number} 行为空", "重新运行 prepare_formic_job.py")
        raw = parse_json_text(text, f"{source}:第 {line_number} 行")
        if not isinstance(raw, dict):
            fail(str(source), f"第 {line_number} 行不是 JSON object", "重新运行 prepare_formic_job.py")
        allowed = {"unit", "files"} if "files" in raw else {"unit", "file", "start", "end"}
        validate_object_keys(raw, f"{source}:第 {line_number} 行", allowed)
        unit = raw.get("unit")
        if not isinstance(unit, int) or isinstance(unit, bool) or unit <= 0:
            fail(str(source), f"第 {line_number} 行 unit 不是正整数", "重新运行 prepare_formic_job.py")
        if unit in seen:
            fail(str(source), f"unit {unit} 重复", "重新运行 prepare_formic_job.py")
        seen.add(unit)
        if "files" in raw:
            files = require_list(raw.get("files"), str(source), f"第 {line_number} 行 files")
            if not files or any(not isinstance(item, str) or not item for item in files):
                fail(str(source), f"第 {line_number} 行 files 不是非空 string array", "重新生成作业")
        units.append(unit)
    return units


def _candidate_lines(results_root: Path) -> tuple[list[str], list[str], list[int]]:
    resolved_root = require_directory(results_root, "Formic results 目录")
    for entry in resolved_root.iterdir():
        if entry.is_dir():
            fail(
                str(entry),
                "Formic results 包含目录",
                "只保留 results/<自然单元号>.md 和 results/output-schema.json；运行档案应位于 runs/run-N",
            )
    files: list[Path] = []
    for path in safe_walk_files(resolved_root):
        if path.parent != resolved_root:
            fail(
                str(path),
                "Formic results 包含子目录或嵌套文件",
                "只保留 results/<自然单元号>.md；运行档案应位于 runs/run-N",
            )
        if path.name == "output-schema.json":
            continue
        if path.suffix.lower() != ".md" or not path.stem.isdecimal() or int(path.stem) <= 0:
            fail(
                str(path),
                "Formic results 包含非自然单元号 Markdown 或未知扩展",
                "只保留 results/<自然单元号>.md；不要把运行档案或临时文件放入 results",
            )
        files.append(path)
    files.sort(key=lambda path: int(path.stem))
    candidates: list[str] = []
    for path in files:
        for raw_line in path.read_text(encoding="utf-8-sig").splitlines():
            line = _BULLET.sub("", raw_line.strip()).strip("` ")
            if not line or line == "无" or line.startswith("#"):
                continue
            candidates.append(line)
    return candidates, [path.name for path in files], [int(path.stem) for path in files]


def _latest_summary(out_root: Path, expected_count: int) -> tuple[str, dict[str, JsonValue]]:
    runs_root = require_directory(out_root / "runs", "Formic runs 目录")
    runs: list[tuple[int, Path]] = []
    for candidate in runs_root.iterdir():
        match = _RUN.fullmatch(candidate.name)
        if match is not None and candidate.is_dir() and int(match.group(1)) > 0:
            runs.append((int(match.group(1)), candidate))
    if not runs:
        fail(str(runs_root), "没有自然编号的 Formic run", "先运行 Formic v2 作业")
    _, latest = max(runs, key=lambda item: item[0])
    summary_path = latest / "summary.json"
    summary = read_json_object(summary_path, "Formic 最新运行 summary.json", allowed_root=out_root)
    validate_object_keys(summary, str(summary_path), _SUMMARY_FIELDS)
    missing = sorted(_SUMMARY_FIELDS - set(summary))
    if missing:
        fail(str(summary_path), f"运行汇总缺少字段：{', '.join(missing)}", "使用当前 Formic v2 完成或续跑")
    for field in (
        "planned",
        "already_completed",
        "started",
        "published",
        "failed",
        "stopped",
        "not_started",
        "llm_calls",
        "llm_calls_with_provider_usage",
        "llm_calls_without_provider_usage",
    ):
        value = summary[field]
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            fail(str(summary_path), f"{field} 不是非负整数", "使用当前 Formic v2 的完整运行汇总")
    if summary["planned"] != expected_count:
        fail(
            str(summary_path),
            f"planned 为 {summary['planned']}，但当前 plan 有 {expected_count} 个单元",
            "使用与当前 OUT 完全对应的 plan.jsonl",
        )
    planned = cast(int, summary["planned"])
    already_completed = cast(int, summary["already_completed"])
    started = cast(int, summary["started"])
    published = cast(int, summary["published"])
    failed = cast(int, summary["failed"])
    stopped = cast(int, summary["stopped"])
    not_started = cast(int, summary["not_started"])
    llm_calls = cast(int, summary["llm_calls"])
    llm_calls_with_provider_usage = cast(int, summary["llm_calls_with_provider_usage"])
    llm_calls_without_provider_usage = cast(int, summary["llm_calls_without_provider_usage"])
    if planned != already_completed + started + not_started:
        fail(
            str(summary_path),
            "运行汇总不满足 planned = already_completed + started + not_started",
            "保留 OUT，并用当前 Formic v2 对同一 plan 执行 --resume",
        )
    if started != published + failed + stopped:
        fail(
            str(summary_path),
            "运行汇总不满足 started = published + failed + stopped",
            "保留 OUT，并用当前 Formic v2 对同一 plan 执行 --resume",
        )
    if llm_calls != llm_calls_with_provider_usage + llm_calls_without_provider_usage:
        fail(
            str(summary_path),
            "运行汇总不满足 llm_calls = llm_calls_with_provider_usage + llm_calls_without_provider_usage",
            "保留 OUT，并用当前 Formic v2 对同一 plan 执行 --resume",
        )
    public_summary: dict[str, JsonValue] = {field: summary[field] for field in sorted(_SUMMARY_FIELDS)}
    return latest.name, public_summary


def _review(args: argparse.Namespace) -> int:
    entries = read_manual(args.manual)
    expected_units = _plan_units(args.plan)
    output_root = require_directory(args.formic_out, "Formic OUT 根目录")
    protect_outputs(
        [args.output],
        inputs=[args.manual, args.plan, output_root],
        replace=args.replace,
    )
    raw_candidates, files, observed_units = _candidate_lines(output_root / "results")
    latest_run, run_summary = _latest_summary(output_root, len(expected_units))
    expected = set(expected_units)
    observed = set(observed_units)
    unexpected = sorted(observed - expected)
    if unexpected:
        examples = ", ".join(str(unit) for unit in unexpected[:5])
        fail(
            str(output_root / "results"),
            f"发现 {len(unexpected)} 个不属于当前 plan 的完成记录；示例：{examples}",
            "使用与当前 OUT 完全对应的 plan，或改用独立 OUT 目录",
        )
    missing_units = [unit for unit in expected_units if unit not in observed]
    if missing_units:
        examples = ", ".join(str(unit) for unit in missing_units[:5])
        fail(
            str(output_root),
            f"Formic 结果缺失 {len(missing_units)} 个；首个 {missing_units[0]}；示例：{examples}；"
            f"最新 {latest_run} 为 failed={run_summary['failed']}、stopped={run_summary['stopped']}、"
            f"not_started={run_summary['not_started']}",
            "修正失败原因后，对同一 OUT 和原 plan 运行 Formic --resume；不要删除 results 完成记录",
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
        "plan": str(args.plan.resolve()),
        "formic_output_files": files,
        "latest_run": latest_run,
        "latest_run_summary": run_summary,
        "worker_candidate_lines": len(raw_candidates),
        "candidates": accepted,
        "rejected": rejected,
        "agent_review_required": [
            "确认候选是游戏专有的单个名词，而不是资源名、多名词组合、短语、句子或普通词。",
            "删除已有稳定固定译法、不需要全局约束的候选。",
            "确定完全汉化且在全游戏不冲突的译名。",
        ],
    }
    write_json(args.output, result, replace=args.replace)
    print(
        f"已读取 {len(files)} 份 Formic v2 完成记录，候选行 {len(raw_candidates)} 条；"
        f"核对后保留 {len(accepted)} 个多次出现候选。"
    )
    print(f"待 Agent 统一审核：{display_path(args.output)}")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _review(parsed))
