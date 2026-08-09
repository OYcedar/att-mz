#!/usr/bin/env python3
"""追踪一个外部文本来源的活动插件消费和显示证据。"""

from __future__ import annotations

import argparse
import re
import sys
from collections import deque
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    display_path,
    ensure_inside,
    fail,
    protect_outputs,
    run_cli,
    safe_walk_files,
    sanitize_line,
    stable_relative,
    write_json,
)
from att_toolbox.js import (
    function_scope_hints,
    loader_call_on_line,
    scan_javascript,
    static_code_targets,
)
from att_toolbox.rpg import (
    actual_path,
    discover_game,
    iter_string_leaves,
    plugin_script_path,
    read_plugins,
)

_IMAGE_SUFFIXES = frozenset({".bmp", ".gif", ".jpeg", ".jpg", ".png", ".webp", ".rpgmvp", ".rpgmvm"})
_DISPLAY_CALL = re.compile(
    r"\b(?:drawText(?:Ex)?|addCommand|setText|showText|Window_[A-Za-z0-9_]*|addChild|addWindow|createText)\b"
)


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="只读检查指定来源是否有活动插件消费者和面向玩家的显示调用。")
    parser.add_argument("--game", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True, help="游戏内的精确文件路径")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replace", action="store_true")
    return parser


def _resolve_source(argument: Path, content_root: Path) -> Path:
    candidate = argument if argument.is_absolute() else content_root / argument
    source = ensure_inside(candidate, content_root, "文本来源")
    if not source.is_file():
        fail(str(argument), "指定来源不是存在的文件", "传入游戏内已存在的精确文件")
    return source


def _normalized_reference(value: str) -> str:
    normalized = value.replace("\\", "/")
    return normalized.removeprefix("./")


def _display_references(code: str) -> list[dict[str, JsonValue]]:
    matches: list[dict[str, JsonValue]] = []
    for line_number, line in enumerate(code.splitlines(), start=1):
        calls = sorted(set(_DISPLAY_CALL.findall(line)))
        if calls:
            matches.append({"line": line_number, "calls": calls})
    return matches


def _parameter_references(
    parameters: dict[str, JsonValue], exact_values: set[str], weak_values: set[str]
) -> tuple[list[dict[str, JsonValue]], list[dict[str, JsonValue]]]:
    exact: list[dict[str, JsonValue]] = []
    weak: list[dict[str, JsonValue]] = []
    for leaf in iter_string_leaves(parameters):
        normalized = _normalized_reference(leaf.value)
        if normalized in exact_values:
            exact.append({"path": actual_path(leaf.path), "matched": normalized})
            continue
        found = sorted(value for value in weak_values if value and value in normalized)
        if found:
            weak.append({"path": actual_path(leaf.path), "matched_substrings": found})
    return exact, weak


def _display_relation(
    reference_line: int,
    displays: list[dict[str, JsonValue]],
    scopes: dict[int, int | None],
) -> dict[str, JsonValue]:
    display_lines = [item["line"] for item in displays if isinstance(item.get("line"), int)]
    typed_lines = [line for line in display_lines if isinstance(line, int)]
    nearest = min((abs(reference_line - line) for line in typed_lines), default=None)
    reference_scope = scopes.get(reference_line)
    same_scope = [
        line for line in typed_lines if reference_scope is not None and scopes.get(line) == reference_scope
    ]
    return {
        "same_line": reference_line in typed_lines,
        "nearby_within_5_lines": nearest is not None and nearest <= 5,
        "nearest_line_distance": nearest,
        "same_lexical_function_candidate": bool(same_scope),
    }


def _has_uncertain_loader(item: dict[str, JsonValue]) -> bool:
    raw_edges = item.get("loader_edges")
    if not isinstance(raw_edges, list):
        return False
    edges = raw_edges
    return any(isinstance(edge, dict) and edge.get("resolution") != "exact" for edge in edges)


def _trace(args: argparse.Namespace) -> int:
    game = discover_game(args.game)
    source = _resolve_source(args.source, game.content_root)
    protect_outputs(
        [args.output],
        inputs=[source],
        forbidden_roots=[game.supplied_root, game.content_root],
        replace=args.replace,
    )
    relative = stable_relative(source, game.content_root)
    active = [plugin for plugin in read_plugins(game.content_root) if plugin.status]
    exact_values = {
        _normalized_reference(value)
        for value in (relative, source.name, relative.removeprefix("data/"))
        if value
    }
    weak_values = {source.stem}
    consumers: list[dict[str, JsonValue]] = []
    code_files = {
        stable_relative(path, game.content_root): path
        for path in safe_walk_files(game.content_root)
        if path.suffix.lower() in {".js", ".mjs"}
    }
    for plugin in active:
        root_script = plugin_script_path(game.content_root, plugin.name)
        if root_script is None:
            consumers.append(
                {
                    "plugin": plugin.name,
                    "active": True,
                    "script": f"js/plugins/{plugin.name}.js",
                    "source_references": [],
                    "display_calls": [],
                    "script_missing": True,
                }
            )
            continue
        root_relative = stable_relative(root_script, game.content_root)
        queue: deque[tuple[Path, tuple[str, ...]]] = deque([(root_script, (root_relative,))])
        visited: set[Path] = set()
        while queue:
            script, chain = queue.popleft()
            if script in visited:
                continue
            visited.add(script)
            script_relative = stable_relative(script, game.content_root)
            scan = scan_javascript(script.read_text(encoding="utf-8-sig"))
            displays = _display_references(scan.code)
            scopes = function_scope_hints(scan.code)
            exact_references: list[dict[str, JsonValue]] = []
            weak_references: list[dict[str, JsonValue]] = []
            loader_edges: list[JsonValue] = []
            for literal in scan.literals:
                normalized = _normalized_reference(literal.value)
                if not literal.dynamic_template and normalized in exact_values:
                    exact_references.append(
                        {
                            "line": literal.line,
                            "matched": normalized,
                            "literal_kind": literal.kind,
                            "display_relation": _display_relation(literal.line, displays, scopes),
                        }
                    )
                else:
                    found = sorted(value for value in weak_values if value and value in normalized)
                    if found:
                        weak_references.append(
                            {
                                "line": literal.line,
                                "matched_substrings": found,
                                "dynamic_template": literal.dynamic_template,
                            }
                        )
                if literal.dynamic_template or not loader_call_on_line(scan.code, literal.line):
                    continue
                target_candidates = static_code_targets(literal.value, script_relative)
                if not target_candidates:
                    continue
                existing = [target for target in target_candidates if target in code_files]
                resolution = "exact" if len(existing) == 1 else "missing" if not existing else "ambiguous"
                target = existing[0] if resolution == "exact" else None
                cycle = target in chain if target is not None else False
                loader_edges.append(
                    {
                        "line": literal.line,
                        "candidates": list(target_candidates),
                        "resolution": resolution,
                        "cycle": cycle,
                    }
                )
                if target is not None and not cycle:
                    queue.append((code_files[target], (*chain, target)))
            if script == root_script:
                exact_parameters, weak_parameters = _parameter_references(
                    plugin.parameters, exact_values, weak_values
                )
            else:
                exact_parameters, weak_parameters = [], []
            source_is_direct = script == source and script == root_script
            source_is_loaded = script == source and script != root_script
            if (
                exact_references
                or weak_references
                or exact_parameters
                or weak_parameters
                or source_is_direct
                or source_is_loaded
                or loader_edges
                or scan.warnings
            ):
                consumers.append(
                    {
                        "plugin": plugin.name,
                        "active": True,
                        "script": script_relative,
                        "loader_chain": list(chain),
                        "loader_edges": loader_edges,
                        "source_is_active_plugin_script": source_is_direct,
                        "source_is_statically_loaded_code": source_is_loaded,
                        "exact_static_path_references": exact_references,
                        "weak_literal_substring_references": weak_references,
                        "exact_parameter_references": exact_parameters,
                        "weak_parameter_substring_references": weak_parameters,
                        "display_calls": displays,
                        "lexer_warnings": list(scan.warnings),
                        "script_missing": False,
                    }
                )
    referenced_consumers = [
        item
        for item in consumers
        if item.get("source_is_active_plugin_script") is True
        or item.get("source_is_statically_loaded_code") is True
        or (
            isinstance(item.get("exact_static_path_references"), list)
            and item["exact_static_path_references"]
        )
        or (isinstance(item.get("exact_parameter_references"), list) and item["exact_parameter_references"])
    ]
    display_consumers = [
        item
        for item in referenced_consumers
        if isinstance(item.get("display_calls"), list) and item["display_calls"]
    ]
    source_code_evidence: JsonValue = None
    source_code_displays: list[dict[str, JsonValue]] = []
    if source.suffix.lower() in {".js", ".mjs"}:
        source_scan = scan_javascript(source.read_text(encoding="utf-8-sig"))
        source_code_displays = _display_references(source_scan.code)
        source_code_evidence = {
            "display_calls": source_code_displays,
            "literal_count": len(source_scan.literals),
            "lexer_warnings": list(source_scan.warnings),
            "note": "来源脚本内的显示调用仍只证明代码候选，不证明活动调用路径。",
        }
    uncertain_consumers = [
        item
        for item in consumers
        if (
            isinstance(item.get("weak_literal_substring_references"), list)
            and item["weak_literal_substring_references"]
        )
        or (
            isinstance(item.get("weak_parameter_substring_references"), list)
            and item["weak_parameter_substring_references"]
        )
        or _has_uncertain_loader(item)
        or (isinstance(item.get("lexer_warnings"), list) and item["lexer_warnings"])
    ]
    consumer_status = (
        "candidate"
        if referenced_consumers
        else "requires_agent_check"
        if uncertain_consumers
        else "not_found"
    )
    result: dict[str, JsonValue] = {
        "engine": game.engine,
        "source": relative,
        "checks": {
            "inside_game_directory": True,
            "non_image_file": source.suffix.lower() not in _IMAGE_SUFFIXES,
            "active_runtime_consumer": consumer_status,
            "player_display_call_in_consumer": "candidate" if display_consumers else "not_found",
            "player_display_call_in_source_code": "candidate" if source_code_displays else "not_found",
            "builtin_coverage": "requires_agent_check",
            "rules_complete_reversible_mapping": "requires_agent_check",
            "extract_group_unit_write_back_mapping": "requires_agent_check",
            "unique_owner": "requires_agent_check",
        },
        "active_consumer_evidence": consumers,
        "source_code_evidence": source_code_evidence,
        "generic_enabled": False,
        "notes": [
            "文件存在、代码引用或显示 API 各自都不足以证明该文本会向玩家显示。",
            "精确静态字面量和参数命中只标为 candidate；弱子串、动态模板和词法函数范围不能升级为已确认消费者。",
            "只有 Agent 核对实际参数流、运行条件、Builtin/Rules 边界和可逆写回后，才能把这个精确来源分配给 Generic。",
        ],
    }
    write_json(args.output, result, replace=args.replace)
    print(
        f"已检查来源 {sanitize_line(relative)}；找到活动消费者候选 {len(referenced_consumers)} 个，"
        f"其中 {len(display_consumers)} 个包含显示调用候选。"
    )
    print(f"证据：{display_path(args.output)}")
    print("Generic 仍为关闭；本工具不代替可见性、Rules 可表达性和唯一所有者审核。")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _trace(parsed))
