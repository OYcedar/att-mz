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
from att_toolbox.resources import ResourceReference, classify_resource_reference
from att_toolbox.rpg import (
    BUILTIN_DATABASE_FIELDS,
    BUILTIN_EVENT_CODES,
    STANDARD_DATA_FILES,
    actual_path,
    canonical_map_files,
    discover_game,
    is_builtin_data_path,
    iter_event_commands,
    iter_string_leaves,
    load_data_json,
    looks_like_player_text,
    plugin_script_path,
    read_plugins,
    require_game_root,
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
    parser.add_argument(
        "--game",
        type=Path,
        required=True,
        help="游戏安装根；标准 Windows MV 的 www 可由 Game.exe 与 package.json 安全识别",
    )
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


def _resource_fact(
    source: str,
    location: str,
    reference: ResourceReference,
) -> dict[str, JsonValue]:
    return {
        "source": source,
        "location": location,
        "basis": reference.basis,
        "resource_kind": reference.resource_kind,
    }


def _custom_json_fact(
    path: Path, data_root: Path, content_root: Path, *, canonical_map: bool
) -> tuple[dict[str, JsonValue], list[dict[str, JsonValue]]]:
    top_level = path.parent == data_root.resolve(strict=True)
    standard = top_level and path.name in STANDARD_DATA_FILES
    relative = path.relative_to(content_root).as_posix()
    try:
        root = load_data_json(path, content_root)
    except ToolError as error:
        if standard or canonical_map:
            raise
        return (
            {
                "path": relative,
                "kind": "unparsed_data_json",
                "candidate_string_count": None,
                "resource_reference_count": None,
                "builtin": False,
                "rules_supported": False,
                "reason": f"文件扩展名为 .json，但外层内容无法按 JSON 解析：{error.reason}",
                "next_check": "确认实际文件格式和活动消费者，并在所有者审计中明确排除或调查其他唯一所有者",
                "source": relative,
            },
            [],
        )
    if (top_level and path.name == "System.json") or canonical_map:
        if not isinstance(root, dict):
            fail(str(path), "标准 RPG Maker 数据根值不是 object", "修正损坏的标准 data JSON")
    elif standard and not isinstance(root, list):
        fail(str(path), "标准 RPG Maker 数据根值不是 array", "修正损坏的标准 data JSON")
    candidate_string_count = 0
    resource_references: list[dict[str, JsonValue]] = []
    for leaf in iter_string_leaves(root):
        if is_builtin_data_path(path.name if top_level else "", leaf.path, canonical_map=canonical_map):
            continue
        resource = classify_resource_reference(leaf.path, leaf.value)
        if resource is not None:
            resource_references.append(
                _resource_fact(relative, f"{relative}:{actual_path(leaf.path)}", resource)
            )
            continue
        if looks_like_player_text(leaf.value):
            candidate_string_count += 1
    rules_supported = top_level and path.suffix == ".json"
    return (
        {
            "path": relative,
            "kind": (
                "standard_data_uncovered_fields"
                if standard
                else "custom_data"
                if top_level
                else "nested_data_json"
            ),
            "candidate_string_count": candidate_string_count,
            "resource_reference_count": len(resource_references),
            "builtin": False,
            "rules_supported": rules_supported,
            "next_check": (
                "Extract Rules 的确定路径和可逆写回"
                if rules_supported
                else "Rules 只接受 data 根目录的精确 .json 基名；必须明确排除或调查其他所有者"
            ),
            "source": relative,
        },
        resource_references,
    )


def _javascript_fact(
    scan: JavaScriptScan,
    script_relative: str,
    *,
    source: str | None = None,
) -> tuple[dict[str, JsonValue], list[dict[str, JsonValue]]]:
    natural_source = source or script_relative
    display_lines = {
        line_number
        for line_number, line in enumerate(scan.code.splitlines(), start=1)
        if _DISPLAY_CALL.search(line)
    }
    resource_references = [
        _resource_fact(
            natural_source,
            f"{natural_source}:line{literal.line}",
            resource,
        )
        for literal in scan.literals
        if (resource := classify_resource_reference((), literal.value)) is not None
    ]
    positive = [
        literal
        for literal in scan.literals
        if literal.value.strip()
        and classify_resource_reference((), literal.value) is None
        and looks_like_player_text(literal.value)
        and not (
            static_code_targets(literal.value, script_relative)
            and loader_call_on_line(scan.code, literal.line)
        )
        and any(abs(literal.line - display_line) <= 4 for display_line in display_lines)
    ]
    negative = [literal for literal in scan.literals if literal.value.strip() and literal not in positive]
    return (
        {
            "status": "scanned",
            "literal_count": len(scan.literals),
            "player_text_candidate_count": len(positive),
            "resource_reference_count": len(resource_references),
            "positive_sample_locations": [
                {"line": literal.line, "kind": literal.kind, "characters": len(literal.value)}
                for literal in positive[:5]
            ],
            "negative_sample_locations": [
                {"line": literal.line, "kind": literal.kind, "characters": len(literal.value)}
                for literal in negative[:5]
            ],
            "warnings": list(scan.warnings),
        },
        resource_references,
    )


def _game_relative(path: Path, supplied_root: Path) -> str:
    return path.relative_to(supplied_root).as_posix()


def _resource_sort_key(item: dict[str, JsonValue]) -> tuple[str, str, str]:
    return (str(item.get("source")), str(item.get("location")), str(item.get("basis")))


def _inventory(args: argparse.Namespace) -> int:
    game = discover_game(args.game)
    game_root = require_game_root(game)
    protect_outputs(
        [args.output],
        forbidden_roots=[game_root, game.content_root],
        replace=args.replace,
    )
    data_root = game.content_root / "data"
    plugins = read_plugins(game.content_root)
    active_plugins = [plugin for plugin in plugins if plugin.status]

    # 从已确认的完整游戏根扫描一次已经覆盖嵌套内容根；按解析后的文件去重，
    # 让标准 www 推导和显式安装根都保持完整且不会重复计数。
    game_files_by_path = {path.resolve(strict=True): path for path in safe_walk_files(game_root)}
    game_files = sorted(
        game_files_by_path,
        key=lambda item: _game_relative(item, game_root).encode("utf-8"),
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
    resource_references: list[dict[str, JsonValue]] = []
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
            fact, references = _custom_json_fact(path, data_root, game.content_root, canonical_map=False)
            other_data.append(fact)
            resource_references.extend(references)
        elif top_level and path.name == "System.json":
            builtin_files.append({"path": "data/System.json", "non_blank_unit_count": None})
            text_sources.append(
                {"source": "data/System.json:builtin-fields", "kind": "builtin", "builtin": True}
            )
            fact, references = _custom_json_fact(path, data_root, game.content_root, canonical_map=False)
            other_data.append(fact)
            resource_references.extend(references)
        elif path in map_paths or (top_level and path.name in {"CommonEvents.json", "Troops.json"}):
            builtin_files.append({"path": f"data/{path.name}", "non_blank_unit_count": None})
            text_sources.append(
                {"source": f"data/{path.name}:builtin-events", "kind": "builtin", "builtin": True}
            )
            fact, references = _custom_json_fact(
                path,
                data_root,
                game.content_root,
                canonical_map=path in map_paths,
            )
            other_data.append(fact)
            resource_references.extend(references)
        else:
            fact, references = _custom_json_fact(path, data_root, game.content_root, canonical_map=False)
            other_data.append(fact)
            resource_references.extend(references)

    event_counts: Counter[int] = Counter()
    event_sources: dict[int, set[str]] = defaultdict(set)
    parameter_indexes: dict[int, set[int]] = defaultdict(set)
    for command in iter_event_commands(game.content_root):
        event_counts[command.code] += 1
        event_sources[command.code].add(command.source_file)
        parameter_indexes[command.code].update(range(len(command.parameters)))
        for parameter, value in enumerate(command.parameters):
            for leaf in iter_string_leaves(value):
                reference = classify_resource_reference(
                    leaf.path,
                    leaf.value,
                    command_code=command.code,
                    parameter=parameter,
                )
                if reference is not None:
                    resource_references.append(
                        _resource_fact(
                            f"event-command:{command.code}:parameter:{parameter}",
                            f"{command.location}:parameter{parameter}{actual_path(leaf.path)}",
                            reference,
                        )
                    )
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
        candidate_count = 0
        for leaf in iter_string_leaves(plugin.parameters):
            resource = classify_resource_reference(leaf.path, leaf.value)
            if resource is not None:
                resource_references.append(
                    _resource_fact(
                        f"plugin:{plugin.name}:parameters",
                        f"js/plugins.js:plugin{plugin.index + 1}:{plugin.name}:parameters:"
                        f"{actual_path(leaf.path)}",
                        resource,
                    )
                )
            elif looks_like_player_text(leaf.value):
                candidate_count += 1
        script = plugin_script_path(game.content_root, plugin.name)
        if script is None:
            script_fact: dict[str, JsonValue] = {
                "status": "missing",
                "literal_count": 0,
                "player_text_candidate_count": 0,
                "resource_reference_count": 0,
                "positive_sample_locations": [],
                "negative_sample_locations": [],
                "warnings": [{"kind": "active_plugin_script_missing"}],
            }
        else:
            scan = scan_javascript(script.read_text(encoding="utf-8-sig"))
            content_relative = script.relative_to(game.content_root).as_posix()
            script_relative = _game_relative(script, game_root)
            script_fact, references = _javascript_fact(
                scan,
                content_relative,
                source=script_relative,
            )
            resource_references.extend(references)
            active_script_scans.append((plugin.name, content_relative, scan))
            direct_active_scripts.add(script)
        plugin_facts.append(
            {
                "name": plugin.name,
                "index": plugin.index + 1,
                "script": (
                    _game_relative(script, game_root)
                    if script is not None
                    else f"js/plugins/{plugin.name}.js"
                ),
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
        if path.is_relative_to(game.content_root) and path.suffix.lower() in {".js", ".mjs"}
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
        natural_relative = _game_relative(path, game_root)
        fact, references = _javascript_fact(scan, relative, source=natural_relative)
        resource_references.extend(references)
        fact.update(
            {
                "path": natural_relative,
                "format": path.suffix.lower().removeprefix("."),
                "bytes": path.stat().st_size,
                "visibility": "unconfirmed",
                "consumer_validation": "required",
                "active_reference_candidates": active_code_references.get(relative, []),
                "next_check": "核对活动加载关系、显示调用和 Builtin/Rules 可逆边界",
            }
        )
        external_code.append(fact)
        candidate_count = fact.get("player_text_candidate_count")
        warnings = fact.get("warnings")
        if (isinstance(candidate_count, int) and candidate_count > 0) or (
            isinstance(warnings, list) and bool(warnings)
        ):
            text_sources.append(
                {
                    "source": natural_relative,
                    "kind": "external_code",
                    "builtin": False,
                    "rules_supported": False,
                }
            )

    external: list[dict[str, JsonValue]] = []
    for path in game_files:
        if path.suffix.lower() not in _EXTERNAL_TEXT_SUFFIXES:
            continue
        natural_relative = _game_relative(path, game_root)
        content_relative = (
            path.relative_to(game.content_root) if path.is_relative_to(game.content_root) else None
        )
        if (
            content_relative is not None
            and content_relative.parts
            and content_relative.parts[0].lower() == "data"
            and path.suffix.lower() == ".json"
        ):
            continue
        if content_relative is not None and content_relative.as_posix() == "js/plugins.js":
            continue
        fact: dict[str, JsonValue] = {
            "path": natural_relative,
            "format": path.suffix.lower().removeprefix("."),
            "bytes": path.stat().st_size,
            "visibility": "unconfirmed",
            "consumer_validation": "required",
            "next_check": "用 trace_runtime_text.py 核对活动消费者和显示调用",
            "generic_eligibility": "unconfirmed",
        }
        external.append(fact)
        text_sources.append(
            {
                "source": natural_relative,
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
        "game_root": str(game_root),
        "content_root": str(game.content_root),
        "active_plugins": plugin_facts,
        "builtin_sources": builtin_files,
        "data_candidates": other_data,
        "event_commands": event_commands,
        "external_code_candidates": external_code,
        "external_text_candidates": external,
        "resource_references": sorted(resource_references, key=_resource_sort_key),
        "text_sources": [unique_sources[key] for key in sorted(unique_sources)],
        "summary": {
            "active_plugins": len(active_plugins),
            "canonical_maps": len(maps),
            "data_json_files": len(data_files),
            "external_code_candidates": len(external_code),
            "external_text_candidates": len(external),
            "resource_references": len(resource_references),
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
