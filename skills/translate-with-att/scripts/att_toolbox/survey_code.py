"""活动插件参数、递归代码依赖和 JavaScript 字面量证据。"""

from __future__ import annotations

import re
import unicodedata
from bisect import bisect_left, bisect_right
from collections import Counter
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from heapq import heapify, heappop, heappush
from html.parser import HTMLParser
from pathlib import Path, PurePosixPath
from typing import cast
from urllib.parse import unquote, urlsplit

from att_skill_tools import JsonValue, ToolError, parse_json_text

from .js import (
    JavaScriptScan,
    function_scope_hints,
    loader_call_for_literal,
    scan_javascript,
    static_code_targets,
)
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
_JAVASCRIPT_IDENTIFIER = r"[A-Za-z_$][A-Za-z0-9_$]*"
_PLUGIN_SCHEMA_BLOCK = re.compile(r"(?ms)^[ \t]*/\*:(?P<body>.*?)^[ \t]*\*/")
_PLUGIN_STRUCT_SCHEMA_BLOCK = re.compile(
    r"(?ms)^[ \t]*/\*~struct~(?P<name>[^:\r\n]+):(?P<body>.*?)^[ \t]*\*/"
)
_PLUGIN_SCHEMA_DIRECTIVE = re.compile(
    r"(?m)^[ \t]*(?:\*[ \t]*)?@(?P<name>param|type)[ \t]+(?P<value>.*?)[ \t]*$"
)
ReadOnce = Callable[[Path], tuple[bytes, FileSnapshot]]
_HTML_LINE_BREAK = re.compile(r"\r\n|\r|\n")


@dataclass(frozen=True, slots=True)
class _MainHtmlResolution:
    html: Path
    package: Path
    package_document: Mapping[str, JsonValue]


@dataclass(frozen=True, slots=True)
class BootstrapTitleConsumers:
    package: Path
    main_html: Path
    package_window_title: bool
    main_html_title_line: int | None
    main_html_title_lines: frozenset[int]


@dataclass(frozen=True, slots=True)
class _OpenHtmlTitle:
    start: int
    content_start: int
    attrs: tuple[tuple[str, str | None], ...]


@dataclass(frozen=True, slots=True)
class _HtmlTitleElement:
    start: int
    content_start: int
    closing_start: int
    end: int
    attrs: tuple[tuple[str, str | None], ...]


@dataclass(frozen=True, slots=True)
class _MainHtmlTitleMatch:
    line: int
    whole_line: bool


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


_HTML_RAW_TEXT_ELEMENTS = frozenset(
    {"script", "style", "textarea", "xmp", "iframe", "noembed", "noframes", "noscript"}
)
_HTML_FOREIGN_ELEMENTS = frozenset({"svg", "math"})


class _TitleElementParser(HTMLParser):
    def __init__(self, text: str) -> None:
        super().__init__(convert_charrefs=False)
        self._text = text
        self._parser_line_starts = [
            0,
            *(index + 1 for index, character in enumerate(text) if character == "\n"),
        ]
        self.title_starts = 0
        self.elements: list[_HtmlTitleElement] = []
        self._open: list[_OpenHtmlTitle] = []
        self._ignored: list[tuple[str, str]] = []

    def _position(self) -> int:
        line, column = self.getpos()
        return self._parser_line_starts[line - 1] + column

    def _ignore_start(self, normalized: str, *, self_closing: bool) -> bool:
        if self._ignored:
            if self._ignored[-1][1] in {"raw", "plaintext"}:
                return True
            if normalized == "plaintext":
                self._ignored.append((normalized, "plaintext"))
            elif normalized in _HTML_RAW_TEXT_ELEMENTS:
                self._ignored.append((normalized, "raw"))
            elif normalized == "template" or (not self_closing and normalized in _HTML_FOREIGN_ELEMENTS):
                self._ignored.append((normalized, "subtree"))
            return True
        if normalized == "plaintext":
            self._ignored.append((normalized, "plaintext"))
        elif normalized in _HTML_RAW_TEXT_ELEMENTS:
            self._ignored.append((normalized, "raw"))
        elif normalized == "template" or (not self_closing and normalized in _HTML_FOREIGN_ELEMENTS):
            self._ignored.append((normalized, "subtree"))
        return bool(self._ignored)

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        normalized = tag.casefold()
        if self._ignore_start(normalized, self_closing=False):
            return
        if normalized != "title":
            return
        start = self._position()
        raw_tag = self.get_starttag_text() or ""
        self.title_starts += 1
        self._open.append(
            _OpenHtmlTitle(
                start=start,
                content_start=start + len(raw_tag),
                attrs=tuple(attrs),
            )
        )

    def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        normalized = tag.casefold()
        if self._ignore_start(normalized, self_closing=True):
            return
        if normalized == "title":
            self.title_starts += 1

    def handle_endtag(self, tag: str) -> None:
        normalized = tag.casefold()
        if self._ignored:
            ignored_name, ignored_mode = self._ignored[-1]
            if ignored_mode != "plaintext" and normalized == ignored_name:
                self._ignored.pop()
            return
        if normalized != "title" or not self._open:
            return
        closing_start = self._position()
        closing_end = self._text.find(">", closing_start)
        if closing_end < 0:
            return
        opening = self._open.pop()
        self.elements.append(
            _HtmlTitleElement(
                start=opening.start,
                content_start=opening.content_start,
                closing_start=closing_start,
                end=closing_end + 1,
                attrs=opening.attrs,
            )
        )


def _package_main_html(
    package: Path,
    existing: Mapping[Path, Path],
    game_root: Path,
    read_once: ReadOnce,
) -> _MainHtmlResolution | None:
    raw, _snapshot = read_once(existing[package])
    root = parse_json_text(decode_text(raw, existing[package]), str(existing[package]))
    if not isinstance(root, dict) or not isinstance(root.get("main"), str):
        return None
    main = cast(str, root["main"])
    split = urlsplit(main)
    if split.scheme or split.netloc:
        return None
    target = (existing[package].parent / unquote(split.path)).resolve(strict=False)
    if (
        target not in existing
        or not target.is_relative_to(game_root)
        or target.suffix.lower() not in {".htm", ".html"}
    ):
        return None
    return _MainHtmlResolution(
        html=existing[target],
        package=existing[package],
        package_document=root,
    )


def _main_html_resolution(
    game: GameInfo,
    game_root: Path,
    files: Sequence[Path],
    read_once: ReadOnce,
) -> _MainHtmlResolution | None:
    existing = {path.resolve(strict=True): path for path in files}
    candidates: list[Path] = []
    for root in (game.supplied_root, game.content_root, game_root):
        package = (root / "package.json").resolve(strict=False)
        if package in existing and package not in candidates:
            candidates.append(package)
    for package in candidates:
        if (resolved := _package_main_html(package, existing, game_root, read_once)) is not None:
            return resolved
    return None


def _main_html(
    game: GameInfo,
    game_root: Path,
    files: Sequence[Path],
    read_once: ReadOnce,
) -> Path | None:
    resolved = _main_html_resolution(game, game_root, files, read_once)
    if resolved is not None:
        return resolved.html
    existing = {path.resolve(strict=True): path for path in files}
    fallback = (game.content_root / "index.html").resolve(strict=False)
    return existing.get(fallback)


def _line_number_at(text: str, position: int) -> int:
    return 1 + sum(1 for _match in _HTML_LINE_BREAK.finditer(text, 0, position))


def _matching_main_html_title(text: str, game_title: str) -> _MainHtmlTitleMatch | None:
    parser = _TitleElementParser(text)
    parser.feed(text)
    parser.close()
    if parser.title_starts != 1 or len(parser.elements) != 1:
        return None
    element = parser.elements[0]
    if (
        element.attrs
        or text[element.start : element.content_start] != "<title>"
        or text[element.content_start : element.closing_start] != game_title
        or text[element.closing_start : element.end] != "</title>"
    ):
        return None
    opening_line = _line_number_at(text, element.start)
    closing_line = _line_number_at(text, element.end)
    line_start_match = list(_HTML_LINE_BREAK.finditer(text, 0, element.start))
    line_start = line_start_match[-1].end() if line_start_match else 0
    line_end_match = _HTML_LINE_BREAK.search(text, element.end)
    line_end = line_end_match.start() if line_end_match is not None else len(text)
    before_title = text[line_start : element.start]
    after_title = text[element.end : line_end]
    return _MainHtmlTitleMatch(
        line=opening_line,
        whole_line=(
            opening_line == closing_line
            and is_structural_blank(before_title)
            and is_structural_blank(after_title)
        ),
    )


def bootstrap_title_consumers(
    game_root: Path,
    files: Sequence[Path],
    read_once: ReadOnce,
    game_title: str,
) -> BootstrapTitleConsumers | None:
    """确认标准 bootstrap 中仍与非空 System 标题同源的派生消费者。"""

    if not game_title:
        return None
    existing = {path.resolve(strict=True): path for path in files}
    root_package = (game_root / "package.json").resolve(strict=False)
    if root_package not in existing:
        return None
    package_path = existing[root_package]
    raw_package, _snapshot = read_once(package_path)
    package_document = parse_json_text(decode_text(raw_package, package_path), str(package_path))
    if not isinstance(package_document, dict):
        return None
    main = package_document.get("main")
    if not isinstance(main, str) or not _is_rust_bootstrap_main(main):
        return None
    target = (game_root / Path(*main.split("/"))).resolve(strict=False)
    if target not in existing or not target.is_relative_to(game_root):
        return None
    resolved = _MainHtmlResolution(
        html=existing[target],
        package=package_path,
        package_document=package_document,
    )
    window = resolved.package_document.get("window")
    package_title_matches = isinstance(window, dict) and window.get("title") == game_title
    raw_html, _snapshot = read_once(resolved.html)
    html = decode_text(raw_html, resolved.html)
    html_title = _matching_main_html_title(html, game_title)
    return BootstrapTitleConsumers(
        package=resolved.package.resolve(strict=True),
        main_html=resolved.html.resolve(strict=True),
        package_window_title=package_title_matches,
        main_html_title_line=html_title.line if html_title is not None else None,
        main_html_title_lines=(
            frozenset({html_title.line}) if html_title is not None and html_title.whole_line else frozenset()
        ),
    )


def _is_rust_bootstrap_main(main: str) -> bool:
    if (
        not main
        or main.startswith("/")
        or main.endswith("/")
        or "//" in main
        or "\\" in main
        or ":" in main
        or any(unicodedata.category(character) == "Cc" for character in main)
    ):
        return False
    segments = main.split("/")
    if any(not segment or segment in {".", ".."} or segment.endswith((".", " ")) for segment in segments):
        return False
    return Path(segments[-1]).suffix == ".html"


def _direct_html_code_roots(
    game: GameInfo,
    game_root: Path,
    files: Sequence[Path],
    code_lookup: Mapping[str, str],
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
        target = game.content_root / decoded.lstrip("/") if decoded.startswith("/") else html.parent / decoded
        try:
            target = target.resolve(strict=True)
        except OSError:
            continue
        if not target.is_relative_to(game.content_root):
            continue
        actual_relative = code_lookup.get(str(target))
        if actual_relative is None:
            continue
        relative = actual_relative
        normalized = relative.casefold()
        parts = normalized.split("/")
        name = parts[-1]
        standard = (
            normalized == "js/plugins.js"
            or (len(parts) >= 2 and parts[0] == "js" and parts[1] == "libs")
            or (parts and parts[0] == "js" and name.startswith(("rpg_", "rmmz_")))
        )
        if not standard:
            roots.add(relative)
    return tuple(sorted(roots, key=lambda value: value.encode("utf-8")))


def _has_line_within(sorted_lines: Sequence[int], line: int, radius: int) -> bool:
    position = bisect_left(sorted_lines, line - radius)
    return position < len(sorted_lines) and sorted_lines[position] <= line + radius


def _plugin_evidence_index(
    plugin: PluginInfo, scan: JavaScriptScan
) -> dict[str, tuple[set[str], list[dict[str, JsonValue]]]]:
    """一次建立全部参数的启发式引用索引，避免每个 leaf 重扫插件源码。"""

    parameter_names = set(plugin.parameters)
    matched: dict[str, set[int]] = {name: set() for name in parameter_names}
    lines = scan.code.splitlines()
    for line_number, line in enumerate(lines, start=1):
        for match in re.finditer(_JAVASCRIPT_IDENTIFIER, line):
            identifier = match.group(0)
            if identifier in parameter_names:
                matched[identifier].add(line_number)
    for literal in scan.literals:
        if literal.value in parameter_names:
            matched[literal.value].add(literal.line)

    display_source_lines = sorted(
        line_number for line_number, line in enumerate(lines, start=1) if _DISPLAY_CALL.search(line)
    )
    protocol_source_lines = sorted(
        line_number for line_number, line in enumerate(lines, start=1) if _PROTOCOL_USE.search(line)
    )
    result: dict[str, tuple[set[str], list[dict[str, JsonValue]]]] = {}
    for parameter_name, raw_lines in matched.items():
        matched_lines = sorted(raw_lines)
        roles = {"unknown"}
        display_lines = [line for line in matched_lines if _has_line_within(display_source_lines, line, 3)]
        protocol_lines = [line for line in matched_lines if _has_line_within(protocol_source_lines, line, 2)]
        if display_lines:
            roles.add("display_candidate")
        if protocol_lines:
            roles.add("protocol_candidate")
        evidence: list[dict[str, JsonValue]] = []
        if matched_lines:
            direct_display_sinks = sorted(
                {
                    match.group(0)
                    for line in matched_lines
                    for match in _DISPLAY_CALL.finditer(lines[line - 1])
                }
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
        result[parameter_name] = (roles, evidence)
    return result


def _literal_tokenized_code(scan: JavaScriptScan) -> tuple[str, dict[str, str]]:
    """用不可混淆 token 还原字符串位置，同时继续屏蔽注释和字符串正文。"""

    prefix = "__ATT_SURVEY_STRING_"
    while prefix in scan.code:
        prefix = f"_{prefix}"
    pieces: list[str] = []
    values: dict[str, str] = {}
    cursor = 0
    for number, literal in enumerate(
        sorted(
            (literal for literal in scan.literals if literal.start is not None and literal.end is not None),
            key=lambda literal: cast(int, literal.start),
        )
    ):
        start = cast(int, literal.start)
        end = cast(int, literal.end)
        if start < cursor:
            continue
        token = f"{prefix}{number}__"
        pieces.append(scan.code[cursor:start])
        pieces.append(token)
        pieces.append("".join(character for character in scan.code[start:end] if character in "\r\n"))
        values[token] = literal.value
        cursor = end
    pieces.append(scan.code[cursor:])
    return "".join(pieces), values


@dataclass(frozen=True, slots=True)
class _SchemaType:
    base: str
    array_depth: int

    @property
    def struct_name(self) -> str | None:
        match = re.fullmatch(r"struct<([^<>\r\n]+)>", self.base)
        return match.group(1) if match is not None else None

    @property
    def serialized(self) -> bool:
        return self.array_depth > 0 or self.struct_name is not None

    def element(self) -> _SchemaType | None:
        return _SchemaType(self.base, self.array_depth - 1) if self.array_depth > 0 else None

    def display(self) -> str:
        return self.base + "[]" * self.array_depth


def _parse_schema_type(value: str) -> _SchemaType | None:
    match = re.fullmatch(
        r"(?P<base>struct<[^<>\r\n]+>|[A-Za-z_][A-Za-z0-9_]*)(?P<arrays>(?:\[\])*)",
        value.strip(),
    )
    if match is None:
        return None
    return _SchemaType(match.group("base"), len(match.group("arrays")) // 2)


def _declared_parameter_types(blocks: Sequence[str]) -> dict[str, _SchemaType | None]:
    result: dict[str, _SchemaType | None] = {}
    for body in blocks:
        parameter: str | None = None
        for directive in _PLUGIN_SCHEMA_DIRECTIVE.finditer(body):
            name = directive.group("name")
            value = directive.group("value").strip()
            if name == "param":
                parameter = value or None
                continue
            parsed = _parse_schema_type(value)
            if parameter is None or parsed is None:
                parameter = None
                continue
            if parameter not in result:
                result[parameter] = parsed
            elif result[parameter] != parsed:
                result[parameter] = None
            parameter = None
    return result


def _schema_types(
    source: str,
) -> tuple[dict[str, _SchemaType | None], dict[str, dict[str, _SchemaType | None]]]:
    root = _declared_parameter_types([match.group("body") for match in _PLUGIN_SCHEMA_BLOCK.finditer(source)])
    struct_blocks: dict[str, list[str]] = {}
    for match in _PLUGIN_STRUCT_SCHEMA_BLOCK.finditer(source):
        name = match.group("name").strip()
        if name:
            struct_blocks.setdefault(name, []).append(match.group("body"))
    structs = {name: _declared_parameter_types(blocks) for name, blocks in struct_blocks.items()}
    return root, structs


def _assignment_counts(code: str) -> Counter[str]:
    identifier = rf"(?P<post>{_JAVASCRIPT_IDENTIFIER})"
    prefix_identifier = rf"(?P<prefix>{_JAVASCRIPT_IDENTIFIER})"
    pattern = re.compile(
        rf"(?<![A-Za-z0-9_$\.]){identifier}(?![A-Za-z0-9_$])\s*"
        r"(?:=(?!=|>)|\+=|-=|\*=|/=|%=|\+\+|--)"
        rf"|(?:\+\+|--)\s*{prefix_identifier}(?![A-Za-z0-9_$])"
    )
    counts: Counter[str] = Counter()
    for match in pattern.finditer(code):
        counts[match.group("post") or match.group("prefix")] += 1
    return counts


def _line_starts(code: str) -> list[int]:
    return [0, *(index + 1 for index, character in enumerate(code) if character == "\n")]


def _line_at(starts: Sequence[int], position: int) -> int:
    return bisect_right(starts, position)


def _json_parse_serialized_parameters(plugin: PluginInfo, scan: JavaScriptScan) -> set[str]:
    """只接受从当前插件参数对象直接流入 JSON.parse 的静态关系。"""

    code, literal_values = _literal_tokenized_code(scan)
    literal_tokens = tuple(literal_values)
    if not literal_tokens:
        return set()
    token_pattern = "(?:" + "|".join(re.escape(token) for token in literal_tokens) + ")"
    binding = re.compile(
        rf"\b(?:const|let|var)\s+(?P<alias>{_JAVASCRIPT_IDENTIFIER})\s*=\s*"
        rf"PluginManager\s*\.\s*parameters\s*\(\s*(?P<plugin>{token_pattern})\s*\)"
    )
    scopes = function_scope_hints(code)
    counts = _assignment_counts(code)
    line_starts = _line_starts(code)
    aliases: dict[str, int | None] = {}
    for match in binding.finditer(code):
        alias = match.group("alias")
        if literal_values[match.group("plugin")] != plugin.name or counts[alias] != 1:
            continue
        line = _line_at(line_starts, match.start())
        aliases[alias] = scopes.get(line)
    parse_access = re.compile(
        r"\bJSON\s*\.\s*parse\s*\(\s*\(*\s*(?:"
        r"PluginManager\s*\.\s*parameters\s*\(\s*(?P<plugin>"
        + token_pattern
        + rf")\s*\)|(?P<alias>{_JAVASCRIPT_IDENTIFIER}))\s*"
        rf"(?:\.\s*(?P<dot>{_JAVASCRIPT_IDENTIFIER})(?![A-Za-z0-9_$])|"
        r"\[\s*(?P<bracket>" + token_pattern + rf")\s*\])\s*(?:(?:\|\||\?\?)\s*{token_pattern}\s*)?\)*\s*\)"
    )
    result: set[str] = set()
    for match in parse_access.finditer(code):
        plugin_token = match.group("plugin")
        alias = match.group("alias")
        if plugin_token is not None:
            if literal_values[plugin_token] != plugin.name:
                continue
        elif alias is None or alias not in aliases:
            continue
        else:
            line = _line_at(line_starts, match.start())
            if scopes.get(line) != aliases[alias]:
                continue
        bracket = match.group("bracket")
        parameter_name = match.group("dot") if bracket is None else literal_values[bracket]
        if parameter_name in plugin.parameters:
            result.add(parameter_name)
    return result


@dataclass(frozen=True, slots=True)
class _ParameterSerializationPlan:
    parameter: str
    schema_type: _SchemaType | None
    structs: Mapping[str, Mapping[str, _SchemaType | None]]
    direct_json_parse: bool
    evidence: tuple[dict[str, JsonValue], ...]

    def _type_at(self, path: tuple[str | int, ...]) -> _SchemaType | None:
        current = self.schema_type
        if current is None or not path or path[0] != self.parameter:
            return None
        for step in path[1:]:
            element = current.element()
            if element is not None:
                if not isinstance(step, int):
                    return None
                current = element
                continue
            struct_name = current.struct_name
            if struct_name is None or not isinstance(step, str):
                return None
            current = self.structs.get(struct_name, {}).get(step)
            if current is None:
                return None
        return current

    def should_decode(self, path: tuple[str | int, ...], decode_positions: tuple[int, ...]) -> bool:
        if len(path) in decode_positions:
            return False
        expected = self._type_at(path)
        if expected is not None and expected.serialized:
            return True
        return self.direct_json_parse and path == (self.parameter,)


def _serialized_parameter_plans(
    plugin: PluginInfo, scan: JavaScriptScan | None, source: str | None
) -> dict[str, _ParameterSerializationPlan]:
    if not plugin.status or scan is None or source is None:
        return {}
    root_types, structs = _schema_types(source)
    parsed_parameters = _json_parse_serialized_parameters(plugin, scan)
    result: dict[str, _ParameterSerializationPlan] = {}
    for parameter in plugin.parameters:
        schema_type = root_types.get(parameter)
        direct = parameter in parsed_parameters
        if (schema_type is None or not schema_type.serialized) and not direct:
            continue
        evidence: list[dict[str, JsonValue]] = []
        if schema_type is not None and schema_type.serialized:
            evidence.append(
                {
                    "kind": "plugin_parameter_serialized_consumer",
                    "basis": "plugin_schema",
                    "schema_type": schema_type.display(),
                    "analysis_status": "confirmed",
                }
            )
        if direct:
            evidence.append(
                {
                    "kind": "plugin_parameter_serialized_consumer",
                    "basis": "json_parse",
                    "data_flow": "plugin_parameter_direct_argument",
                    "analysis_status": "confirmed",
                }
            )
        result[parameter] = _ParameterSerializationPlan(
            parameter,
            schema_type,
            structs,
            direct,
            tuple(evidence),
        )
    return result


def _opaque_json_container(value: JsonValue, serialization_evidence: Sequence[object]) -> bool:
    if serialization_evidence or not isinstance(value, str):
        return False
    try:
        decoded = parse_json_text(value, "未绑定消费者的插件参数")
    except ToolError:
        return False
    return isinstance(decoded, (dict, list))


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
    plugin_sources: dict[str, str] = {}
    code_scans: dict[str, JavaScriptScan] = {}
    code_paths: dict[str, Path] = {}
    code_lookup: dict[str, str] = {}
    for path in files:
        if not path.is_relative_to(resolved_content) or path.suffix.lower() not in {".js", ".mjs"}:
            continue
        relative_content = path.relative_to(game.content_root).as_posix()
        physical_identity = str(path.resolve(strict=True))
        code_lookup[physical_identity] = relative_content
        code_paths[relative_content] = path
    for relative in _direct_html_code_roots(
        game,
        game_root,
        files,
        code_lookup,
        read_once,
    ):
        script = code_paths[relative]
        raw, _snapshot = read_once(script)
        code_scans[relative] = scan_javascript(decode_text(raw, script))
    for plugin in active_plugins:
        if plugin.name in plugin_scans:
            continue
        script = plugin_script_path(game.content_root, plugin.name)
        if script is None:
            continue
        actual_relative = code_lookup.get(str(script.resolve(strict=True)))
        if actual_relative is None:
            continue
        script = code_paths[actual_relative]
        raw, _snapshot = read_once(script)
        source = decode_text(raw, script)
        scan = scan_javascript(source)
        plugin_scans[plugin.name] = scan
        plugin_sources[plugin.name] = source
        code_scans[actual_relative] = scan

    # 参数值及其通用消费者证据。
    for plugin in plugins:
        scan = plugin_scans.get(plugin.name)
        serialized_parameters = _serialized_parameter_plans(
            plugin,
            scan,
            plugin_sources.get(plugin.name),
        )
        consumer_index = _plugin_evidence_index(plugin, scan) if scan is not None else {}
        consumer_default: tuple[set[str], list[dict[str, JsonValue]]] = (
            ({"unknown"}, [{"kind": "active_plugin_script_missing"}])
            if plugin.status and scan is None
            else ({"unknown"}, [])
        )
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
        for top_parameter_name, parameter_value in plugin.parameters.items():
            serialization_plan = serialized_parameters.get(top_parameter_name)
            serialization_evidence = (
                list(serialization_plan.evidence) if serialization_plan is not None else []
            )
            opaque_json_container = _opaque_json_container(
                parameter_value,
                serialization_evidence,
            )
            decode_error: ToolError | None = None
            try:
                parameter_leaves = list(
                    iter_string_leaves(
                        parameter_value,
                        path=(top_parameter_name,),
                        decode_serialized_at=(
                            serialization_plan.should_decode if serialization_plan is not None else None
                        ),
                    )
                )
            except ToolError as error:
                decode_error = error
                parameter_leaves = list(
                    iter_string_leaves(
                        parameter_value,
                        path=(top_parameter_name,),
                    )
                )
            for leaf in parameter_leaves:
                if not leaf.path or not isinstance(leaf.path[0], str):
                    raise AssertionError("插件参数字符串缺少参数名")
                parameter_name = leaf.path[0]
                location = (
                    f"plugins.js:plugin{plugin.index + 1}:{plugin.name}:parameters:{actual_path(leaf.path)}"
                )
                plugins_relative = plugins_js.relative_to(game_root).as_posix()
                physical_decode_positions = tuple(position + 2 for position in leaf.decode_positions)
                if decode_error is not None:
                    indexed_roles, indexed_evidence = consumer_index.get(parameter_name, consumer_default)
                    roles, evidence = set(indexed_roles), list(indexed_evidence)
                    locations.append(
                        LocationFact(
                            source=f"plugin:{plugin.name}:parameters",
                            location=location,
                            source_text=leaf.value,
                            classification="review",
                            physical_file=plugins_relative,
                            json_path=(plugin.index, "parameters", *leaf.path),
                            roles={*roles, "protocol_candidate"},
                            evidence=[
                                *evidence,
                                *serialization_evidence,
                                {
                                    "kind": "serialized_plugin_parameter_invalid",
                                    "reason": decode_error.reason,
                                    "analysis_status": "confirmed",
                                },
                            ],
                        )
                    )
                    continue
                if opaque_json_container:
                    indexed_roles, indexed_evidence = consumer_index.get(parameter_name, consumer_default)
                    roles, evidence = set(indexed_roles), list(indexed_evidence)
                    locations.append(
                        LocationFact(
                            source=f"plugin:{plugin.name}:parameters",
                            location=location,
                            source_text=leaf.value,
                            classification="review",
                            physical_file=plugins_relative,
                            json_path=(plugin.index, "parameters", *leaf.path),
                            roles={*roles, "protocol_candidate"},
                            evidence=[
                                *evidence,
                                {
                                    "kind": "json_container_without_confirmed_consumer",
                                    "analysis_status": "unknown",
                                },
                            ],
                        )
                    )
                    continue
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
                    indexed_roles, indexed_evidence = consumer_index.get(parameter_name, consumer_default)
                    roles, evidence = set(indexed_roles), list(indexed_evidence)
                    evidence.extend(serialization_evidence)
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
                        roles=roles,
                        evidence=[*evidence, *rule_evidence, *lexical_suggestion(leaf.value)],
                    )
                )

    # 静态路径只建立活动候选关系，不把“被引用”误写成运行时已显示。
    active_code = set(code_scans)
    pending_code = [(relative.encode("utf-8"), relative) for relative in active_code]
    heapify(pending_code)
    while pending_code:
        _sort_key, relative = heappop(pending_code)
        scan = code_scans[relative]
        for literal in scan.literals:
            if literal.dynamic_template or not loader_call_for_literal(scan.code, literal):
                continue
            for target in static_code_targets(literal.value, relative):
                candidate_path = game.content_root.joinpath(*PurePosixPath(target).parts)
                try:
                    physical_identity = str(candidate_path.resolve(strict=True))
                except OSError:
                    continue
                actual_target = code_lookup.get(physical_identity)
                if actual_target is None or actual_target in active_code:
                    continue
                target_path = code_paths[actual_target]
                raw, _snapshot = read_once(target_path)
                code_scans[actual_target] = scan_javascript(decode_text(raw, target_path))
                active_code.add(actual_target)
                heappush(pending_code, (actual_target.encode("utf-8"), actual_target))
    for relative in sorted(active_code):
        path = code_paths.get(relative)
        if path is None:
            continue
        scan = code_scans.get(relative)
        if scan is None:
            raise AssertionError(f"活动代码尚未扫描：{relative}")
        code_lines = scan.code.splitlines()
        display_lines = sorted(
            line_number for line_number, line in enumerate(code_lines, start=1) if _DISPLAY_CALL.search(line)
        )
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
            near_display = _has_line_within(display_lines, literal.line, 4)
            path_reference = bool(static_code_targets(literal.value, relative)) and loader_call_for_literal(
                scan.code, literal
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
