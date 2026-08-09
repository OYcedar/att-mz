#!/usr/bin/env python3
"""展开 RPG Maker JSON、插件参数和事件参数，制作 Extract Rules 审核候选。"""

from __future__ import annotations

import argparse
import json
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
    protect_outputs,
    read_json_object,
    require_list,
    run_cli,
    safe_walk_files,
    toml_string,
    validate_object_keys,
    write_json_with_optional_text,
)
from att_toolbox.rpg import (
    BUILTIN_EVENT_CODES,
    actual_path,
    canonical_map_files,
    discover_game,
    is_builtin_data_path,
    iter_event_commands,
    iter_string_leaves,
    load_data_json,
    looks_like_player_text,
    normalized_path,
    read_plugins,
)

_RULE_FIELDS = {"file", "plugin", "code", "parameter", "path", "decode_json", "pattern"}
_SAFE_RULE_FILE = re.compile(r'[^\x00-\x1f<>:"/\\|?*]+\.json\Z')


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(
        description="穷尽扫描 Rules 可表达的字符串路径；只把 Agent 选定的规则写入 TOML。"
    )
    parser.add_argument("--game", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="Extract Rules 候选 JSON")
    parser.add_argument("--decisions", type=Path, help='Agent 审核 JSON：{"rules": [...]}')
    parser.add_argument("--rules-output", type=Path, help="审核后的 Extract Rules TOML")
    parser.add_argument("--replace", action="store_true")
    return parser


def _candidate_key(source: dict[str, JsonValue], path: str, decoded_layers: int) -> str:
    parts = [f"{key}={source[key]}" for key in sorted(source)]
    return "|".join([*parts, f"path={path}", f"decoded={decoded_layers}"])


def _add_candidate(
    grouped: dict[str, dict[str, JsonValue]],
    *,
    source: dict[str, JsonValue],
    path: str,
    actual: str,
    value: str,
    decoded_layers: int,
    location: str,
) -> None:
    if not looks_like_player_text(value):
        return
    key = _candidate_key(source, path, decoded_layers)
    entry = grouped.setdefault(
        key,
        {
            "source": source,
            "path": path,
            "decoded_layers_observed": decoded_layers,
            "occurrences": 0,
            "locations": [],
            "examples": [],
            "runtime_visibility": "unconfirmed",
        },
    )
    entry["occurrences"] = cast(int, entry["occurrences"]) + 1
    locations = cast(list[JsonValue], entry["locations"])
    locations.append(f"{location}{actual}")
    examples = cast(list[JsonValue], entry["examples"])
    if value not in examples and len(examples) < 5:
        examples.append(value)


def _scan_file_candidates(
    content_root: Path,
    grouped: dict[str, dict[str, JsonValue]],
) -> list[JsonValue]:
    data_root = content_root / "data"
    resolved_data = data_root.resolve(strict=True)
    maps = set(canonical_map_files(data_root))
    unsupported: list[JsonValue] = []
    for file_path in sorted(
        (path for path in safe_walk_files(data_root) if path.suffix.lower() == ".json"),
        key=lambda item: item.relative_to(data_root).as_posix().encode("utf-8"),
    ):
        root = load_data_json(file_path, content_root)
        relative = file_path.relative_to(content_root).as_posix()
        rules_supported = file_path.parent == resolved_data and file_path.suffix == ".json"
        unsupported_paths: dict[str, int] = {}
        for leaf in iter_string_leaves(root):
            if is_builtin_data_path(
                file_path.name if file_path.parent == resolved_data else "",
                leaf.path,
                canonical_map=file_path in maps,
            ):
                continue
            path = normalized_path(leaf.path)
            if not path:
                continue
            if not looks_like_player_text(leaf.value):
                continue
            if not rules_supported:
                unsupported_paths[path] = unsupported_paths.get(path, 0) + 1
                continue
            _add_candidate(
                grouped,
                source={"type": "file", "file": file_path.name},
                path=path,
                actual=actual_path(leaf.path),
                value=leaf.value,
                decoded_layers=leaf.decoded_layers,
                location=f"{relative}:",
            )
        if unsupported_paths:
            unsupported.append(
                {
                    "source": relative,
                    "kind": "nested_or_noncanonical_data_json",
                    "candidate_string_count": sum(unsupported_paths.values()),
                    "candidate_paths": [
                        {"path": path, "occurrences": unsupported_paths[path]}
                        for path in sorted(unsupported_paths)
                    ],
                    "rules_supported": False,
                    "reason": "Extract Rules 的 file 只接受 data 根目录内精确小写 .json 基名",
                    "next_check": "在所有者审计中明确排除或调查其他唯一所有者",
                }
            )
    return unsupported


def _scan_plugin_candidates(content_root: Path, grouped: dict[str, dict[str, JsonValue]]) -> None:
    for plugin in read_plugins(content_root):
        if not plugin.status:
            continue
        for leaf in iter_string_leaves(plugin.parameters):
            path = normalized_path(leaf.path)
            if not path:
                continue
            _add_candidate(
                grouped,
                source={"type": "plugin", "plugin": plugin.name},
                path=path,
                actual=actual_path(leaf.path),
                value=leaf.value,
                decoded_layers=leaf.decoded_layers,
                location=f"js/plugins.js:{plugin.name}:",
            )


def _scan_command_candidates(content_root: Path, grouped: dict[str, dict[str, JsonValue]]) -> None:
    for command in iter_event_commands(content_root):
        if command.code in BUILTIN_EVENT_CODES:
            continue
        for parameter, value in enumerate(command.parameters):
            for leaf in iter_string_leaves(value):
                path = normalized_path(leaf.path)
                _add_candidate(
                    grouped,
                    source={"type": "command", "code": command.code, "parameter": parameter},
                    path=path,
                    actual=actual_path(leaf.path),
                    value=leaf.value,
                    decoded_layers=leaf.decoded_layers,
                    location=f"{command.location}:parameter{parameter}",
                )


def _load_rules(path: Path) -> list[dict[str, JsonValue]]:
    root = read_json_object(path, "Extract Rules 审核文件")
    validate_object_keys(root, str(path), {"rules"})
    raw_rules = require_list(root.get("rules"), str(path), "rules")
    result: list[dict[str, JsonValue]] = []
    seen: set[str] = set()
    for number, raw in enumerate(raw_rules, start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"第 {number} 条 rule 不是 object", "把每条 rule 写成 JSON object")
        rule = raw
        validate_object_keys(rule, f"{path}:第 {number} 条 rule", _RULE_FIELDS)
        source_count = (
            int("file" in rule) + int("plugin" in rule) + int("code" in rule or "parameter" in rule)
        )
        if source_count != 1 or ("code" in rule) != ("parameter" in rule):
            fail(
                str(path),
                f"第 {number} 条 rule 没有恰好选择 file、plugin 或 code+parameter 一种来源",
                "按 RPG Maker Rules 规格修正来源字段",
            )
        for field in ("file", "plugin", "path", "pattern"):
            if field in rule and (not isinstance(rule[field], str) or rule[field] == ""):
                fail(str(path), f"第 {number} 条 rule 的 {field} 不是非空 string", f"修正 {field}")
        file_name = rule.get("file")
        if isinstance(file_name, str) and _SAFE_RULE_FILE.fullmatch(file_name) is None:
            fail(
                str(path),
                f"第 {number} 条 rule 的 file 不是 data 根目录内安全小写 .json 基名",
                "不要用目录路径或非小写扩展；嵌套 data JSON 不由 Extract Rules 拥有",
            )
        for field in ("code", "parameter"):
            if field in rule and (
                not isinstance(rule[field], int)
                or isinstance(rule[field], bool)
                or cast(int, rule[field]) < 0
            ):
                fail(str(path), f"第 {number} 条 rule 的 {field} 不是非负整数", f"修正 {field}")
        if "decode_json" in rule and not isinstance(rule["decode_json"], bool):
            fail(str(path), f"第 {number} 条 rule 的 decode_json 不是 boolean", "改为 true 或 false")
        if ("file" in rule or "plugin" in rule) and "path" not in rule:
            fail(str(path), f"第 {number} 条 file/plugin rule 缺少 path", "填写确定路径")
        identity = json.dumps(rule, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        if identity in seen:
            fail(str(path), f"第 {number} 条 rule 与前面的规则完全重复", "删除重复规则")
        seen.add(identity)
        result.append(rule)
    return result


def _rules_toml(rules: list[dict[str, JsonValue]]) -> str:
    if not rules:
        return "rule = []\n"
    chunks: list[str] = []
    field_order = ("file", "plugin", "code", "parameter", "path", "decode_json", "pattern")
    for rule in rules:
        lines = ["[[rule]]"]
        for field in field_order:
            if field not in rule:
                continue
            value = rule[field]
            if isinstance(value, str):
                rendered = toml_string(value)
            elif isinstance(value, bool):
                rendered = "true" if value else "false"
            elif isinstance(value, int):
                rendered = str(value)
            else:
                fail("审核规则", f"字段 {field} 无法写入 TOML", "修正审核 JSON 字段类型")
            lines.append(f"{field} = {rendered}")
        chunks.append("\n".join(lines))
    return "\n\n".join(chunks) + "\n"


def _analyze(args: argparse.Namespace) -> int:
    decisions_path = cast(Path | None, args.decisions)
    rules_output = cast(Path | None, args.rules_output)
    if (decisions_path is None) != (rules_output is None):
        fail("命令行参数", "--decisions 与 --rules-output 必须同时提供", "同时提供审核 JSON 和规则输出路径")
    rules = _load_rules(decisions_path) if decisions_path is not None else None
    game = discover_game(args.game)
    targets = [args.output] if rules_output is None else [args.output, rules_output]
    inputs = [] if decisions_path is None else [decisions_path]
    protect_outputs(
        targets,
        inputs=inputs,
        forbidden_roots=[game.supplied_root, game.content_root],
        replace=args.replace,
    )
    grouped: dict[str, dict[str, JsonValue]] = {}
    unsupported_sources = _scan_file_candidates(game.content_root, grouped)
    _scan_plugin_candidates(game.content_root, grouped)
    _scan_command_candidates(game.content_root, grouped)
    candidates = [grouped[key] for key in sorted(grouped)]
    for number, candidate in enumerate(candidates, start=1):
        candidate["candidate_number"] = number
    result: dict[str, JsonValue] = {
        "engine": game.engine,
        "content_root": str(game.content_root),
        "candidates": candidates,
        "unsupported_sources": unsupported_sources,
        "summary": {
            "candidate_paths": len(candidates),
            "unsupported_sources": len(unsupported_sources),
        },
        "decision_required": True,
        "notes": [
            "候选只表示字符串路径可由 Rules 表达，不证明这些文本会向玩家显示。",
            "嵌套 data JSON 与非小写 .json 文件会单独列为 unsupported_sources，不能扁平化为同名基名。",
            "examples 最多保留 5 个不同原值；occurrences 和 locations 仍基于全量扫描。",
            "最终 TOML 必须交给 ATT 完成严格 TOML、PCRE2、来源、冲突和可逆写回验收。",
        ],
    }
    write_json_with_optional_text(
        args.output,
        result,
        text_path=rules_output,
        text=_rules_toml(rules) if rules is not None else None,
        replace=args.replace,
    )
    print(
        f"已递归检查全部 data JSON、活动插件参数和非 Builtin 事件命令；"
        f"Rules 不支持来源 {len(unsupported_sources)} 个。"
    )
    print(f"发现 {len(candidates)} 个需要审核的确定路径：{display_path(args.output)}")
    if rules is not None and rules_output is not None:
        print(f"已按 Agent 审核写入 {len(rules)} 条 Extract Rules：{display_path(rules_output)}")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _analyze(parsed))
