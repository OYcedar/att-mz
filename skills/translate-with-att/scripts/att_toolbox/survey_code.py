"""活动插件参数、递归代码依赖和 JavaScript 字面量证据。"""

from __future__ import annotations

import re
from collections.abc import Callable, Mapping, Sequence
from html.parser import HTMLParser
from pathlib import Path
from typing import cast
from urllib.parse import unquote, urlsplit

from att_skill_tools import JsonValue, parse_json_text

from .js import JavaScriptScan, loader_call_on_line, scan_javascript, static_code_targets
from .resources import classify_resource_reference
from .rpg import GameInfo, PluginInfo, actual_path, iter_string_leaves, normalized_path, plugin_script_path
from .rpg_control_codes import is_structural_blank
from .survey_identity import rule_manual_id
from .survey_io import decode_text
from .survey_model import FileSnapshot, LocationFact
from .survey_suggestions import lexical_suggestion, rule_proposal

_DISPLAY_CALL = re.compile(
    r"\b(?:drawText(?:Ex)?|addCommand|setText|showText|addChild|addWindow|createText|"
    r"Window_[A-Za-z0-9_]*)\b"
)
_PROTOCOL_USE = re.compile(
    r"(?:===|!==|==|!=|\bcase\b|\bswitch\b|\.indexOf\s*\(|\.includes\s*\(|\bfilter\s*\()"
)
_LOG_CALL = re.compile(r"\b(?:console\.(?:log|warn|error|debug)|print|trace)\s*\(")
ReadOnce = Callable[[Path], tuple[bytes, FileSnapshot]]


class _ScriptSourceParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.sources: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag.casefold() != "script":
            return
        for name, value in attrs:
            if name.casefold() == "src" and value:
                self.sources.append(value)


def _main_html(
    game: GameInfo,
    game_root: Path,
    files: Sequence[Path],
    read_once: ReadOnce,
) -> Path | None:
    existing = {path.resolve(strict=True): path for path in files}
    candidates: list[Path] = []
    for root in (game.supplied_root, game.content_root, game_root):
        package = (root / "package.json").resolve(strict=False)
        if package in existing and package not in candidates:
            candidates.append(package)
    for package in candidates:
        raw, _snapshot = read_once(existing[package])
        root = parse_json_text(decode_text(raw, existing[package]), str(existing[package]))
        if not isinstance(root, dict) or not isinstance(root.get("main"), str):
            continue
        main = cast(str, root["main"])
        split = urlsplit(main)
        if split.scheme or split.netloc:
            continue
        target = (existing[package].parent / unquote(split.path)).resolve(strict=False)
        if (
            target in existing
            and target.is_relative_to(game_root)
            and target.suffix.lower() in {".htm", ".html"}
        ):
            return existing[target]
    fallback = (game.content_root / "index.html").resolve(strict=False)
    return existing.get(fallback)


def _direct_html_code_roots(
    game: GameInfo,
    game_root: Path,
    files: Sequence[Path],
    code_paths: Mapping[str, Path],
    read_once: ReadOnce,
) -> tuple[str, ...]:
    html = _main_html(game, game_root, files, read_once)
    if html is None:
        return ()
    raw, _snapshot = read_once(html)
    parser = _ScriptSourceParser()
    parser.feed(decode_text(raw, html))
    roots: set[str] = set()
    for source in parser.sources:
        split = urlsplit(source)
        if split.scheme or split.netloc or not split.path:
            continue
        decoded = unquote(split.path)
        target = (
            game.content_root / decoded.lstrip("/") if decoded.startswith("/") else html.parent / decoded
        ).resolve(strict=False)
        if not target.is_relative_to(game.content_root):
            continue
        relative = target.relative_to(game.content_root).as_posix()
        if relative not in code_paths:
            continue
        parts = Path(relative).parts
        name = parts[-1]
        standard = (
            relative == "js/plugins.js"
            or (len(parts) >= 2 and parts[0] == "js" and parts[1] == "libs")
            or (parts and parts[0] == "js" and name.startswith(("rpg_", "rmmz_")))
            or relative == "js/main.js"
        )
        if not standard:
            roots.add(relative)
    return tuple(sorted(roots, key=lambda value: value.encode("utf-8")))


def plugin_evidence(
    plugin: PluginInfo, scan: JavaScriptScan | None, parameter_name: str
) -> tuple[set[str], list[dict[str, JsonValue]]]:
    roles = {"unknown"}
    if scan is None:
        return roles, [{"kind": "active_plugin_script_missing"}]
    lines = scan.code.splitlines()
    matched_lines = sorted(
        {
            *(index for index, line in enumerate(lines, start=1) if parameter_name in line),
            *(literal.line for literal in scan.literals if literal.value == parameter_name),
        }
    )
    display_lines = [
        line
        for line in matched_lines
        if any(
            _DISPLAY_CALL.search(lines[index - 1])
            for index in range(max(1, line - 3), min(len(lines), line + 3) + 1)
        )
    ]
    protocol_lines = [
        line
        for line in matched_lines
        if any(
            _PROTOCOL_USE.search(lines[index - 1])
            for index in range(max(1, line - 2), min(len(lines), line + 2) + 1)
        )
    ]
    if display_lines:
        roles.add("display_candidate")
    if protocol_lines:
        roles.add("protocol_candidate")
    evidence: list[dict[str, JsonValue]] = []
    if matched_lines:
        direct_display_sinks = sorted(
            {match.group(0) for line in matched_lines for match in _DISPLAY_CALL.finditer(lines[line - 1])}
        )
        direct_protocol = any(_PROTOCOL_USE.search(lines[line - 1]) for line in matched_lines)
        consumer_fingerprint = [f"display:{sink}" for sink in direct_display_sinks] + (
            ["protocol:comparison"] if direct_protocol else []
        )
        if not consumer_fingerprint and display_lines:
            consumer_fingerprint.append("display:nearby")
        if not consumer_fingerprint and protocol_lines:
            consumer_fingerprint.append("protocol:nearby")
        evidence.append(
            {
                "kind": "plugin_parameter_reference",
                "plugin_index": plugin.index + 1,
                "parameter": parameter_name,
                "reference_count": len(matched_lines),
                "sample_lines": matched_lines[:5],
                "near_display_lines": display_lines[:5],
                "near_protocol_lines": protocol_lines[:5],
                "consumer_fingerprint": consumer_fingerprint or ["reference:unknown"],
                "analysis_status": "heuristic",
            }
        )
    return roles, evidence


def scan_code_sources(
    game: GameInfo,
    game_root: Path,
    files: Sequence[Path],
    plugins: Sequence[PluginInfo],
    plugins_js: Path,
    locations: list[LocationFact],
    read_once: ReadOnce,
) -> tuple[dict[str, JavaScriptScan], set[Path]]:
    active_plugins = [plugin for plugin in plugins if plugin.status]
    resolved_content = game.content_root.resolve(strict=True)
    plugin_scans: dict[str, JavaScriptScan] = {}
    code_scans: dict[str, JavaScriptScan] = {}
    code_paths: dict[str, Path] = {}
    for path in files:
        if not path.is_relative_to(resolved_content) or path.suffix.lower() not in {".js", ".mjs"}:
            continue
        relative_content = path.relative_to(game.content_root).as_posix()
        code_paths[relative_content] = path
    for relative in _direct_html_code_roots(game, game_root, files, code_paths, read_once):
        script = code_paths[relative]
        raw, _snapshot = read_once(script)
        code_scans[relative] = scan_javascript(decode_text(raw, script))
    for plugin in active_plugins:
        if plugin.name in plugin_scans:
            continue
        script = plugin_script_path(game.content_root, plugin.name)
        if script is None:
            continue
        raw, _snapshot = read_once(script)
        scan = scan_javascript(decode_text(raw, script))
        plugin_scans[plugin.name] = scan
        code_scans[script.relative_to(game.content_root).as_posix()] = scan

    # 参数值及其通用消费者证据。
    for plugin in plugins:
        scan = plugin_scans.get(plugin.name)
        for field_name, field_value in (
            ("name", plugin.name),
            ("description", plugin.description),
        ):
            metadata_location = f"plugins.js:plugin{plugin.index + 1}:{field_name}"
            metadata_classification = (
                "structural_whitespace" if is_structural_blank(field_value) else "review"
            )
            locations.append(
                LocationFact(
                    source="plugins.js:metadata",
                    location=metadata_location,
                    source_text=field_value,
                    classification=metadata_classification,
                    physical_file=plugins_js.relative_to(game_root).as_posix(),
                    json_path=(plugin.index, field_name),
                    roles={"structure"} if is_structural_blank(field_value) else {"plugin_metadata"},
                    evidence=(
                        [
                            {
                                "kind": "plugin_metadata_field",
                                "active": plugin.status,
                                "analysis_status": "confirmed",
                            },
                            *lexical_suggestion(field_value),
                        ]
                        if not is_structural_blank(field_value)
                        else []
                    ),
                    generic_kind="plugins_js_field",
                    generic_locator={"plugin": plugin.index + 1, "field": field_name},
                )
            )
        for leaf in iter_string_leaves(plugin.parameters):
            if not leaf.path or not isinstance(leaf.path[0], str):
                raise AssertionError("插件参数字符串缺少参数名")
            parameter_name = leaf.path[0]
            location = (
                f"plugins.js:plugin{plugin.index + 1}:{plugin.name}:parameters:{actual_path(leaf.path)}"
            )
            plugins_relative = plugins_js.relative_to(game_root).as_posix()
            physical_decode_positions = tuple(position + 2 for position in leaf.decode_positions)
            if is_structural_blank(leaf.value):
                locations.append(
                    LocationFact(
                        source=f"plugin:{plugin.name}:parameters",
                        location=location,
                        source_text=leaf.value,
                        classification="structural_whitespace",
                        physical_file=plugins_relative,
                        json_path=(plugin.index, "parameters", *leaf.path),
                        decode_positions=physical_decode_positions,
                        roles={"structure"},
                    )
                )
                continue
            resource = classify_resource_reference(leaf.path, leaf.value)
            if plugin.status:
                roles, evidence = plugin_evidence(plugin, scan, parameter_name)
            else:
                roles = {"unknown"}
                evidence = [
                    {
                        "kind": "inactive_plugin_configuration",
                        "active": False,
                        "analysis_status": "confirmed",
                    }
                ]
            if resource is not None:
                locations.append(
                    LocationFact(
                        source=f"plugin:{plugin.name}:parameters",
                        location=location,
                        source_text=leaf.value,
                        classification="resource_reference",
                        physical_file=plugins_relative,
                        json_path=(plugin.index, "parameters", *leaf.path),
                        decode_positions=physical_decode_positions,
                        resource={
                            "basis": resource.basis,
                            "resource_kind": resource.resource_kind,
                        },
                        roles={"resource"},
                    )
                )
                continue
            if plugin.status:
                rule, rule_evidence = rule_proposal(
                    {"plugin": plugin.name, "path": normalized_path(leaf.path)},
                    leaf.value,
                )
                expected_manual_id = rule_manual_id(
                    leaf.path,
                    leaf.decode_positions,
                    source_file="plugins.js",
                    plugin=plugin,
                )
            else:
                rule = None
                rule_evidence = []
                expected_manual_id = None
            locations.append(
                LocationFact(
                    source=f"plugin:{plugin.name}:parameters",
                    location=location,
                    source_text=leaf.value,
                    classification="review",
                    physical_file=plugins_relative,
                    json_path=(plugin.index, "parameters", *leaf.path),
                    decode_positions=physical_decode_positions,
                    rule=rule,
                    expected_manual_id=expected_manual_id,
                    manual_type="fixed",
                    roles=roles,
                    evidence=[*evidence, *rule_evidence, *lexical_suggestion(leaf.value)],
                )
            )

    # 静态路径只建立活动候选关系，不把“被引用”误写成运行时已显示。
    active_code = set(code_scans)
    pending_code = sorted(active_code)
    while pending_code:
        relative = pending_code.pop(0)
        scan = code_scans[relative]
        for literal in scan.literals:
            if literal.dynamic_template:
                continue
            for target in static_code_targets(literal.value, relative):
                if target not in code_paths or target in active_code:
                    continue
                target_path = code_paths[target]
                raw, _snapshot = read_once(target_path)
                code_scans[target] = scan_javascript(decode_text(raw, target_path))
                active_code.add(target)
                pending_code.append(target)
        pending_code.sort(key=lambda value: value.encode("utf-8"))
    for relative in sorted(active_code):
        path = code_paths.get(relative)
        if path is None:
            continue
        scan = code_scans.get(relative)
        if scan is None:
            raise AssertionError(f"活动代码尚未扫描：{relative}")
        code_lines = scan.code.splitlines()
        display_lines = {
            line_number for line_number, line in enumerate(code_lines, start=1) if _DISPLAY_CALL.search(line)
        }
        for literal_number, literal in enumerate(scan.literals, start=1):
            natural_relative = path.relative_to(game_root).as_posix()
            location = f"{natural_relative}:line{literal.line}:literal{literal_number}"
            locator: dict[str, JsonValue] = {
                "line": literal.line,
                "start": literal.start,
                "end": literal.end,
                "quote": literal.quote,
            }
            if is_structural_blank(literal.value):
                locations.append(
                    LocationFact(
                        source=natural_relative,
                        location=location,
                        source_text=literal.value,
                        classification="structural_whitespace",
                        physical_file=natural_relative,
                        roles={"structure"},
                        generic_kind="javascript_literal",
                        generic_locator=locator,
                    )
                )
                continue
            resource = classify_resource_reference((), literal.value)
            if resource is not None:
                locations.append(
                    LocationFact(
                        source=natural_relative,
                        location=location,
                        source_text=literal.value,
                        classification="resource_reference",
                        physical_file=natural_relative,
                        resource={
                            "basis": resource.basis,
                            "resource_kind": resource.resource_kind,
                        },
                        roles={"resource"},
                        generic_kind="javascript_literal",
                        generic_locator=locator,
                    )
                )
                continue
            near_display = any(abs(literal.line - line) <= 4 for line in display_lines)
            path_reference = bool(static_code_targets(literal.value, relative)) and loader_call_on_line(
                scan.code, literal.line
            )
            line_text = code_lines[literal.line - 1] if literal.line <= len(code_lines) else ""
            direct_sinks = sorted({match.group(0) for match in _DISPLAY_CALL.finditer(line_text)})
            log_sinks = sorted({match.group(0) for match in _LOG_CALL.finditer(line_text)})
            protocol_use = _PROTOCOL_USE.search(line_text) is not None
            if direct_sinks:
                fingerprint = [f"display:{sink}" for sink in direct_sinks]
            elif log_sinks:
                fingerprint = [f"log:{sink}" for sink in log_sinks]
            elif path_reference:
                fingerprint = ["loader:path"]
            elif protocol_use:
                fingerprint = ["protocol:comparison"]
            elif near_display:
                fingerprint = ["display:nearby"]
            else:
                fingerprint = ["unknown"]
            roles = {"display_candidate" if direct_sinks or near_display else "unknown"}
            evidence: list[dict[str, JsonValue]] = [
                {
                    "kind": "javascript_literal_consumer",
                    "active_script_candidate": True,
                    "near_display_call": near_display,
                    "direct_display_sinks": direct_sinks,
                    "log_sinks": log_sinks,
                    "protocol_comparison": protocol_use,
                    "loader_path_literal": path_reference,
                    "consumer_fingerprint": fingerprint,
                    "analysis_status": "heuristic",
                }
            ]
            evidence.extend(lexical_suggestion(literal.value))
            locations.append(
                LocationFact(
                    source=natural_relative,
                    location=location,
                    source_text=literal.value,
                    classification="review",
                    physical_file=natural_relative,
                    roles=roles,
                    evidence=evidence,
                    generic_kind="javascript_literal",
                    generic_locator=locator,
                )
            )

    # 安装根和内容根中的文本容器按精确文件、精确行保留；没有消费者证据时只能进入审核。
    scanned_code_paths = {code_paths[relative].resolve(strict=True) for relative in code_scans}
    return code_scans, scanned_code_paths
