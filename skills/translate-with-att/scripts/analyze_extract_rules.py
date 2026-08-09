#!/usr/bin/env python3
"""展开 RPG Maker JSON、插件参数和事件参数，制作 Extract Rules 审核候选。"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    ToolError,
    atomic_write_text,
    display_path,
    fail,
    preflight_atomic_text_outputs,
    protect_outputs,
    read_json_object,
    require_list,
    run_cli,
    safe_walk_files,
    toml_string,
    validate_object_keys,
    write_json,
)
from att_toolbox.resources import classify_resource_reference
from att_toolbox.rpg import (
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
    parser.add_argument("--manifest-output", type=Path, help="规则自然序号到 inventory 来源的 JSON")
    parser.add_argument("--inventory", type=Path, help="同一游戏的 inspect_rpg_maker.py 输出")
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
        relative = file_path.relative_to(content_root).as_posix()
        top_level = file_path.parent == resolved_data
        canonical_map = file_path in maps
        try:
            root = load_data_json(file_path, content_root)
        except ToolError as error:
            if canonical_map or (top_level and file_path.name in STANDARD_DATA_FILES):
                raise
            unsupported.append(
                {
                    "source": relative,
                    "kind": "unparsed_data_json",
                    "candidate_string_count": None,
                    "candidate_paths": [],
                    "rules_supported": False,
                    "reason": f"文件扩展名为 .json，但外层内容无法按 JSON 解析：{error.reason}",
                    "next_check": "确认实际文件格式和活动消费者，并在所有者审计中明确排除或调查其他唯一所有者",
                }
            )
            continue
        rules_supported = top_level and file_path.suffix == ".json"
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
            if classify_resource_reference(leaf.path, leaf.value) is not None:
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
            if classify_resource_reference(leaf.path, leaf.value) is not None:
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
                if (
                    classify_resource_reference(
                        leaf.path,
                        leaf.value,
                        command_code=command.code,
                        parameter=parameter,
                    )
                    is not None
                ):
                    continue
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


def _inventory_sources(path: Path, game_content_root: Path, engine: str) -> dict[str, dict[str, JsonValue]]:
    root = read_json_object(path, "RPG Maker inventory")
    inventory_engine = root.get("engine")
    inventory_content_root = root.get("content_root")
    if inventory_engine != engine:
        fail(str(path), "inventory 的引擎与当前游戏不一致", "重新对当前游戏运行 inspect_rpg_maker.py")
    if not isinstance(inventory_content_root, str) or Path(inventory_content_root).resolve(
        strict=True
    ) != game_content_root.resolve(strict=True):
        fail(
            str(path),
            "inventory 的内容根与当前游戏不一致",
            "重新对当前精确游戏目录运行 inspect_rpg_maker.py",
        )
    sources: dict[str, dict[str, JsonValue]] = {}
    for number, raw in enumerate(require_list(root.get("text_sources"), str(path), "text_sources"), start=1):
        if not isinstance(raw, dict):
            fail(str(path), f"第 {number} 个 text_source 不是 object", "重新运行 inspect_rpg_maker.py")
        source = raw.get("source")
        if not isinstance(source, str) or not source:
            fail(str(path), f"第 {number} 个 text_source 缺少 source", "重新运行 inspect_rpg_maker.py")
        if source in sources:
            fail(str(path), f"inventory 重复列出来源 {source}", "重新运行 inspect_rpg_maker.py")
        sources[source] = raw
    return sources


def _rule_source(rule: dict[str, JsonValue]) -> str:
    file_name = rule.get("file")
    if isinstance(file_name, str):
        return f"data/{file_name}"
    plugin_name = rule.get("plugin")
    if isinstance(plugin_name, str):
        return f"plugin:{plugin_name}:parameters"
    code = rule.get("code")
    parameter = rule.get("parameter")
    if isinstance(code, int) and isinstance(parameter, int):
        return f"event-command:{code}:parameter:{parameter}"
    fail("审核规则", "规则没有可映射的自然来源", "修正审核规则来源字段")


def _manifest(
    rules: list[dict[str, JsonValue]],
    inventory_sources: dict[str, dict[str, JsonValue]],
    toml_text: str,
) -> dict[str, JsonValue]:
    try:
        parsed = tomllib.loads(toml_text)
    except tomllib.TOMLDecodeError as error:
        fail("生成的 Extract Rules", f"TOML 无法重新解析：{error}", "报告当前脚本实现错误")
    parsed_rules = parsed.get("rule", [])
    if not isinstance(parsed_rules, list) or parsed_rules != rules:
        fail(
            "生成的 Extract Rules",
            "重新解析后的规则与审核决定不一致",
            "报告当前脚本实现错误，不要使用这份输出",
        )
    manifest_rules: list[JsonValue] = []
    for rule_number, rule in enumerate(rules, start=1):
        source = _rule_source(rule)
        fact = inventory_sources.get(source)
        if fact is None:
            fail(
                "Extract Rules 审核文件",
                f"第 {rule_number} 条规则映射到 inventory 中不存在的来源 {source}",
                "使用候选 JSON 和同一游戏 inventory 中的精确来源重新审核",
            )
        if fact.get("rules_supported") is False:
            fail(
                "Extract Rules 审核文件",
                f"第 {rule_number} 条规则映射到 inventory 已明确不支持 Rules 的来源 {source}",
                "改为明确排除、继续调查或在 Generic 条件全部成立后分配给 Generic",
            )
        manifest_rules.append({"rule_number": rule_number, "source": source, "rule": rule})
    return {"rules": manifest_rules}


def _write_review_outputs(
    candidate_path: Path,
    candidate: dict[str, JsonValue],
    rules_path: Path,
    rules_text: str,
    manifest_path: Path,
    manifest: dict[str, JsonValue],
    *,
    replace: bool,
) -> None:
    outputs = [candidate_path, rules_path, manifest_path]
    preflight_atomic_text_outputs(outputs, replace=replace)
    write_json(candidate_path, candidate, replace=replace)
    rules_published = False
    try:
        atomic_write_text(rules_path, rules_text, replace=replace)
        rules_published = True
        write_json(manifest_path, manifest, replace=replace)
    except ToolError as error:
        published = f"候选 JSON {candidate_path.resolve(strict=False)}"
        if rules_published:
            published += f" 和 Rules TOML {rules_path.resolve(strict=False)}"
        raise ToolError(
            object_name=error.object_name,
            reason=error.reason,
            impact=f"{published} 已经生效；{error.impact}",
            help_text=error.help_text,
        ) from None


def _analyze(args: argparse.Namespace) -> int:
    decisions_path = cast(Path | None, args.decisions)
    rules_output = cast(Path | None, args.rules_output)
    manifest_output = cast(Path | None, args.manifest_output)
    inventory_path = cast(Path | None, args.inventory)
    review_values = (decisions_path, rules_output, manifest_output, inventory_path)
    if any(value is not None for value in review_values) and not all(
        value is not None for value in review_values
    ):
        fail(
            "命令行参数",
            "--decisions、--rules-output、--manifest-output 与 --inventory 必须同时提供",
            "同时提供审核 JSON、同一游戏 inventory、规则 TOML 和 manifest 输出路径",
        )
    rules = _load_rules(decisions_path) if decisions_path is not None else None
    game = discover_game(args.game)
    targets = (
        [args.output]
        if rules_output is None or manifest_output is None
        else [args.output, rules_output, manifest_output]
    )
    inputs = [] if decisions_path is None or inventory_path is None else [decisions_path, inventory_path]
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
    if (
        rules is not None
        and rules_output is not None
        and manifest_output is not None
        and inventory_path is not None
    ):
        rules_text = _rules_toml(rules)
        manifest = _manifest(
            rules,
            _inventory_sources(inventory_path, game.content_root, game.engine),
            rules_text,
        )
        _write_review_outputs(
            args.output,
            result,
            rules_output,
            rules_text,
            manifest_output,
            manifest,
            replace=args.replace,
        )
    else:
        write_json(args.output, result, replace=args.replace)
    print(
        f"已递归检查全部 data JSON、活动插件参数和非 Builtin 事件命令；"
        f"Rules 不支持来源 {len(unsupported_sources)} 个。"
    )
    print(f"发现 {len(candidates)} 个需要审核的确定路径：{display_path(args.output)}")
    if rules is not None and rules_output is not None and manifest_output is not None:
        print(f"已按 Agent 审核写入 {len(rules)} 条 Extract Rules：{display_path(rules_output)}")
        print(f"规则自然来源 manifest：{display_path(manifest_output)}")
    return 0


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _analyze(parsed))
