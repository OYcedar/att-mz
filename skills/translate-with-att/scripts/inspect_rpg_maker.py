#!/usr/bin/env python3
"""盘点 RPG Maker MV/MZ 内容根和文本来源。"""

from __future__ import annotations

import argparse
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    ToolError,
    display_path,
    fail,
    protect_outputs,
    run_cli,
    safe_walk_files,
    write_json,
)
from att_toolbox.js import JavaScriptScan, loader_call_on_line, scan_javascript, static_code_targets
from att_toolbox.rpg import (
    BUILTIN_DATABASE_FIELDS,
    BUILTIN_EVENT_CODES,
    STANDARD_DATA_FILES,
    canonical_map_files,
    discover_game,
    is_builtin_data_path,
    iter_event_commands,
    iter_string_leaves,
    load_data_json,
    looks_like_player_text,
    plugin_script_path,
    read_plugins,
)

_DISPLAY_CALL = re.compile(
    r"\b(?:drawText(?:Ex)?|addCommand|setText|showText|addChild|addWindow|createText|Window_[A-Za-z0-9_]*)\b"
)

_EXTERNAL_TEXT_SUFFIXES = frozenset(
    {
        ".cfg",
        ".conf",
        ".csv",
        ".htm",
        ".html",
        ".ini",
        ".json",
        ".markdown",
        ".md",
        ".po",
        ".properties",
        ".srt",
        ".toml",
        ".tsv",
        ".txt",
        ".vtt",
        ".xml",
        ".yaml",
        ".yml",
    }
)


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(
        description="识别 MV/MZ 内容根、活动插件、Builtin 来源和需要继续调查的文本来源。"
    )
    parser.add_argument("--game", type=Path, required=True, help="游戏根或实际内容根")
    parser.add_argument("--output", type=Path, required=True, help="inventory.json 输出路径")
    parser.add_argument("--replace", action="store_true", help="替换已存在的输出文件")
    return parser


def _builtin_count(path: Path, content_root: Path) -> int:
    root = load_data_json(path, content_root)
    fields = BUILTIN_DATABASE_FIELDS.get(path.name)
    if fields is None:
        return 0
    if not isinstance(root, list):
        fail(str(path), "Builtin 数据文件根值不是 array", "修正损坏的 RPG Maker data JSON")
    count = 0
    for index, entry in enumerate(root):
        if index == 0 and entry is None:
            continue
        if not isinstance(entry, dict):
            fail(
                str(path),
                f"Builtin 数据第 {index} 项不是 object",
                "修正损坏的标准数据库条目；只有 index 0 可以是 null",
            )
        for field in fields:
            value = entry.get(field)
            if not isinstance(value, str):
                fail(str(path), f"第 {index} 项 {field} 不是 string", "恢复标准 RPG Maker 数据字段")
            if value.strip():
                count += 1
    return count


def _custom_json_fact(
    path: Path, data_root: Path, content_root: Path, *, canonical_map: bool
) -> dict[str, JsonValue]:
    top_level = path.parent == data_root.resolve(strict=True)
    standard = top_level and path.name in STANDARD_DATA_FILES
    relative = path.relative_to(content_root).as_posix()
    try:
        root = load_data_json(path, content_root)
    except ToolError as error:
        if standard or canonical_map:
            raise
        return {
            "path": relative,
            "kind": "unparsed_data_json",
            "candidate_string_count": None,
            "builtin": False,
            "rules_supported": False,
            "reason": f"文件扩展名为 .json，但外层内容无法按 JSON 解析：{error.reason}",
            "next_check": "确认实际文件格式和活动消费者，并在所有者审计中明确排除或调查其他唯一所有者",
            "source": relative,
        }
    if (top_level and path.name == "System.json") or canonical_map:
        if not isinstance(root, dict):
            fail(str(path), "标准 RPG Maker 数据根值不是 object", "修正损坏的标准 data JSON")
    elif standard and not isinstance(root, list):
        fail(str(path), "标准 RPG Maker 数据根值不是 array", "修正损坏的标准 data JSON")
    strings = [
        leaf
        for leaf in iter_string_leaves(root)
        if not is_builtin_data_path(path.name if top_level else "", leaf.path, canonical_map=canonical_map)
        and looks_like_player_text(leaf.value)
    ]
    rules_supported = top_level and path.suffix == ".json"
    return {
        "path": relative,
        "kind": (
            "standard_data_uncovered_fields"
            if standard
            else "custom_data"
            if top_level
            else "nested_data_json"
        ),
        "candidate_string_count": len(strings),
        "builtin": False,
        "rules_supported": rules_supported,
        "next_check": (
            "Extract Rules 的确定路径和可逆写回"
            if rules_supported
            else "Rules 只接受 data 根目录的精确 .json 基名；必须明确排除或调查其他所有者"
        ),
        "source": relative,
    }


def _javascript_fact(scan: JavaScriptScan, script_relative: str) -> dict[str, JsonValue]:
    display_lines = {
        line_number
        for line_number, line in enumerate(scan.code.splitlines(), start=1)
        if _DISPLAY_CALL.search(line)
    }
    positive = [
        literal
        for literal in scan.literals
        if literal.value.strip()
        and looks_like_player_text(literal.value)
        and not (
            static_code_targets(literal.value, script_relative)
            and loader_call_on_line(scan.code, literal.line)
        )
        and any(abs(literal.line - display_line) <= 4 for display_line in display_lines)
    ]
    negative = [literal for literal in scan.literals if literal.value.strip() and literal not in positive]
    return {
        "status": "scanned",
        "literal_count": len(scan.literals),
        "player_text_candidate_count": len(positive),
        "positive_sample_locations": [
            {"line": literal.line, "kind": literal.kind, "characters": len(literal.value)}
            for literal in positive[:5]
        ],
        "negative_sample_locations": [
            {"line": literal.line, "kind": literal.kind, "characters": len(literal.value)}
            for literal in negative[:5]
        ],
        "warnings": list(scan.warnings),
    }


def _inventory(args: argparse.Namespace) -> int:
    game = discover_game(args.game)
    protect_outputs(
        [args.output],
        forbidden_roots=[game.supplied_root, game.content_root],
        replace=args.replace,
    )
    data_root = game.content_root / "data"
    plugins = read_plugins(game.content_root)
    active_plugins = [plugin for plugin in plugins if plugin.status]

    game_files = sorted(
        safe_walk_files(game.content_root),
        key=lambda item: item.relative_to(game.content_root).as_posix().encode("utf-8"),
    )
    data_files = sorted(
        (
            path
            for path in game_files
            if path.is_relative_to(data_root.resolve(strict=True)) and path.suffix.lower() == ".json"
        ),
        key=lambda item: item.relative_to(data_root).as_posix().encode("utf-8"),
    )
    maps = canonical_map_files(data_root)
    map_paths = set(maps)
    builtin_files: list[dict[str, JsonValue]] = []
    other_data: list[dict[str, JsonValue]] = []
    text_sources: list[dict[str, JsonValue]] = []
    for path in data_files:
        top_level = path.parent == data_root.resolve(strict=True)
        if top_level and path.name in BUILTIN_DATABASE_FIELDS:
            count = _builtin_count(path, game.content_root)
            builtin_files.append({"path": f"data/{path.name}", "non_blank_unit_count": count})
            text_sources.append(
                {
                    "source": f"data/{path.name}:builtin-fields",
                    "kind": "builtin",
                    "builtin": True,
                }
            )
            other_data.append(_custom_json_fact(path, data_root, game.content_root, canonical_map=False))
        elif top_level and path.name == "System.json":
            builtin_files.append({"path": "data/System.json", "non_blank_unit_count": None})
            text_sources.append(
                {"source": "data/System.json:builtin-fields", "kind": "builtin", "builtin": True}
            )
            other_data.append(_custom_json_fact(path, data_root, game.content_root, canonical_map=False))
        elif path in map_paths or (top_level and path.name in {"CommonEvents.json", "Troops.json"}):
            builtin_files.append({"path": f"data/{path.name}", "non_blank_unit_count": None})
            text_sources.append(
                {"source": f"data/{path.name}:builtin-events", "kind": "builtin", "builtin": True}
            )
            other_data.append(
                _custom_json_fact(
                    path,
                    data_root,
                    game.content_root,
                    canonical_map=path in map_paths,
                )
            )
        else:
            other_data.append(_custom_json_fact(path, data_root, game.content_root, canonical_map=False))

    event_counts: Counter[int] = Counter()
    event_sources: dict[int, set[str]] = defaultdict(set)
    parameter_indexes: dict[int, set[int]] = defaultdict(set)
    for command in iter_event_commands(game.content_root):
        event_counts[command.code] += 1
        event_sources[command.code].add(command.source_file)
        parameter_indexes[command.code].update(range(len(command.parameters)))
    event_commands: list[dict[str, JsonValue]] = []
    for code in sorted(event_counts):
        event_commands.append(
            {
                "code": code,
                "count": event_counts[code],
                "builtin": code in BUILTIN_EVENT_CODES,
                "parameter_indexes": sorted(parameter_indexes[code]),
                "source_files": sorted(event_sources[code]),
            }
        )
        if code not in BUILTIN_EVENT_CODES:
            for parameter in sorted(parameter_indexes[code]):
                text_sources.append(
                    {
                        "source": f"event-command:{code}:parameter:{parameter}",
                        "kind": "event_command",
                        "builtin": False,
                    }
                )

    plugin_facts: list[dict[str, JsonValue]] = []
    active_script_scans: list[tuple[str, str, JavaScriptScan]] = []
    direct_active_scripts: set[Path] = set()
    for plugin in active_plugins:
        candidate_count = sum(
            1 for leaf in iter_string_leaves(plugin.parameters) if looks_like_player_text(leaf.value)
        )
        script = plugin_script_path(game.content_root, plugin.name)
        script_fact: dict[str, JsonValue]
        if script is None:
            script_fact = {
                "status": "missing",
                "literal_count": 0,
                "player_text_candidate_count": 0,
                "positive_sample_locations": [],
                "negative_sample_locations": [],
                "warnings": [{"kind": "active_plugin_script_missing"}],
            }
        else:
            scan = scan_javascript(script.read_text(encoding="utf-8-sig"))
            script_relative = script.relative_to(game.content_root).as_posix()
            script_fact = _javascript_fact(scan, script_relative)
            active_script_scans.append((plugin.name, script_relative, scan))
            direct_active_scripts.add(script)
        plugin_facts.append(
            {
                "name": plugin.name,
                "index": plugin.index + 1,
                "script": f"js/plugins/{plugin.name}.js",
                "parameter_count": len(plugin.parameters),
                "parameter_candidate_string_count": candidate_count,
                "script_literals": script_fact,
            }
        )
        if candidate_count:
            text_sources.append(
                {
                    "source": f"plugin:{plugin.name}:parameters",
                    "kind": "plugin_parameter",
                    "builtin": False,
                }
            )
        script_candidate_count = script_fact.get("player_text_candidate_count")
        script_warnings = script_fact.get("warnings")
        if (isinstance(script_candidate_count, int) and script_candidate_count > 0) or (
            isinstance(script_warnings, list) and bool(script_warnings)
        ):
            text_sources.append(
                {
                    "source": f"plugin:{plugin.name}:script-literals",
                    "kind": "active_plugin_script_literal",
                    "builtin": False,
                    "rules_supported": False,
                }
            )

    code_relatives = {
        path.relative_to(game.content_root).as_posix(): path
        for path in game_files
        if path.suffix.lower() in {".js", ".mjs"}
    }
    active_code_references: dict[str, list[JsonValue]] = defaultdict(list)
    for plugin_name, script_relative, scan in active_script_scans:
        for literal in scan.literals:
            if literal.dynamic_template:
                continue
            for target in static_code_targets(literal.value, script_relative):
                if target not in code_relatives:
                    continue
                active_code_references[target].append(
                    {
                        "plugin": plugin_name,
                        "line": literal.line,
                        "exact_static_path_literal": True,
                        "loader_call_on_same_line": loader_call_on_line(scan.code, literal.line),
                    }
                )

    external_code: list[dict[str, JsonValue]] = []
    for relative, path in sorted(code_relatives.items()):
        if relative == "js/plugins.js" or path in direct_active_scripts:
            continue
        scan = scan_javascript(path.read_text(encoding="utf-8-sig"))
        fact = _javascript_fact(scan, relative)
        fact.update(
            {
                "path": relative,
                "format": path.suffix.lower().removeprefix("."),
                "bytes": path.stat().st_size,
                "visibility": "unconfirmed",
                "consumer_validation": "required",
                "active_reference_candidates": active_code_references.get(relative, []),
                "next_check": "核对活动加载关系、显示调用和 Builtin/Rules 可逆边界",
            }
        )
        external_code.append(fact)
        text_sources.append(
            {
                "source": relative,
                "kind": "external_code",
                "builtin": False,
                "rules_supported": False,
            }
        )

    external: list[dict[str, JsonValue]] = []
    for path in game_files:
        if path.suffix.lower() not in _EXTERNAL_TEXT_SUFFIXES:
            continue
        relative = path.relative_to(game.content_root)
        if relative.parts and relative.parts[0].lower() == "data" and path.suffix.lower() == ".json":
            continue
        if relative.as_posix() == "js/plugins.js":
            continue
        fact: dict[str, JsonValue] = {
            "path": relative.as_posix(),
            "format": path.suffix.lower().removeprefix("."),
            "bytes": path.stat().st_size,
            "visibility": "unconfirmed",
            "consumer_validation": "required",
            "next_check": "用 trace_runtime_text.py 核对活动消费者和显示调用",
        }
        external.append(fact)
        text_sources.append(
            {
                "source": relative.as_posix(),
                "kind": "external_file",
                "builtin": False,
                "rules_supported": False,
            }
        )

    for fact in other_data:
        count = fact.get("candidate_string_count")
        if (isinstance(count, int) and count > 0) or fact.get("kind") == "unparsed_data_json":
            text_sources.append(
                {
                    "source": str(fact["source"]),
                    "kind": str(fact["kind"]),
                    "builtin": False,
                    "rules_supported": fact.get("rules_supported", False),
                }
            )

    unique_sources: dict[str, dict[str, JsonValue]] = {}
    for source in text_sources:
        unique_sources[str(source["source"])] = source

    result: dict[str, JsonValue] = {
        "engine": game.engine,
        "content_root": str(game.content_root),
        "active_plugins": plugin_facts,
        "builtin_sources": builtin_files,
        "data_candidates": other_data,
        "event_commands": event_commands,
        "external_code_candidates": external_code,
        "external_text_candidates": external,
        "text_sources": [unique_sources[key] for key in sorted(unique_sources)],
        "summary": {
            "active_plugins": len(active_plugins),
            "canonical_maps": len(maps),
            "data_json_files": len(data_files),
            "external_code_candidates": len(external_code),
            "external_text_candidates": len(external),
            "text_sources": len(unique_sources),
        },
    }
    write_json(args.output, result, replace=args.replace)
    print(
        f"已识别 {game.engine.upper()} 内容根：{display_path(game.content_root)}；"
        f"活动插件 {len(active_plugins)} 个，待审计文本来源 {len(unique_sources)} 类。"
    )
    print(f"详细盘点：{display_path(args.output)}")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _inventory(parsed))
