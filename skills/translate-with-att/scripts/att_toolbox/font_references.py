"""RPG Maker 字体资产、别名、消费者上下文与精确文本补丁图。"""

from __future__ import annotations

import json
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from itertools import pairwise
from pathlib import Path, PurePosixPath
from typing import cast
from urllib.parse import unquote

from att_skill_tools import fail, safe_walk_files

from att_toolbox.font_metadata import FontCoverage, check_font_coverage
from att_toolbox.font_transaction import ByteMutation, sha256_bytes
from att_toolbox.js import loader_call_on_line, scan_javascript, static_code_targets
from att_toolbox.rpg import plugin_script_path, read_plugins

FONT_SUFFIXES = frozenset({".eot", ".otf", ".ttf", ".woff", ".woff2"})
_SCANNED_TEXT_SUFFIXES = frozenset(
    {".css", ".htm", ".html", ".ini", ".js", ".json", ".mjs", ".toml", ".txt", ".xml"}
)
_FONT_WORD = re.compile(r"(?i)(?:\.eot|\.otf|\.ttf|\.woff2?)(?:[?#][^\s'\"()<>]*)?")
_CSS_URL = re.compile(r"(?is)\burl\(\s*(?P<quote>['\"]?)(?P<value>[^'\"()\r\n]+)(?P=quote)\s*\)")
_CSS_FONT_FACE = re.compile(r"(?is)@font-face\s*\{(?P<body>.*?)\}")
_CSS_FONT_FAMILY = re.compile(r"(?is)\bfont-family\s*:\s*(?P<value>[^;}]+)")
_CSS_FONT_SRC = re.compile(r"(?is)\bsrc\s*:\s*(?P<value>[^;}]+)")
_CSS_FAMILY_DECLARATION = re.compile(
    r"(?is)\bfont-family\s*:\s*(?P<quote>['\"]?)(?P<value>[^;'\"}\r\n]+)(?P=quote)\s*;"
)
_JS_FONT_LOADER = re.compile(r"(?:\bGraphics\.loadFont|\bFontManager\.load|\bnew\s+FontFace)\s*\(\s*$")
_JS_FONT_CALL = re.compile(r"(?:\bGraphics\.loadFont|\bFontManager\.load|\bnew\s+FontFace)\s*\([^()\r\n;]*$")
_HTML_SCRIPT_SRC = re.compile(
    r"(?is)<script\b[^>]*\bsrc\s*=\s*(?P<quote>['\"])(?P<value>[^'\"<>]+)(?P=quote)"
)
_CSS_FORMAT = re.compile(
    r"(?is)(?:\s|/\*.*?\*/)*format\(\s*(?P<quote>['\"]?)(?P<value>[^'\"()\r\n]+)(?P=quote)\s*\)"
)
_CSS_FORMAT_FUNCTION = re.compile(r"(?is)(?:\s|/\*.*?\*/)*(?P<function>format\([^)]*\))")
_HTML_TAG = re.compile(
    r"(?is)<\s*(?P<closing>/)?\s*(?P<name>[A-Za-z][A-Za-z0-9:-]*)"
    r"(?P<body>(?:\"[^\"]*\"|'[^']*'|[^'\">])*)>"
)


@dataclass(frozen=True, slots=True)
class FontAsset:
    path: Path
    relative_path: str
    size: int
    sha256: str


@dataclass(frozen=True, slots=True)
class FontReference:
    source: str
    line: int
    context: str
    old_asset: str
    new_asset: str
    old_value: str
    new_value: str
    nested_location: str | None = None


@dataclass(frozen=True, slots=True)
class FontAlias:
    value: str
    asset: str
    basis: str
    source: str
    line: int | None


@dataclass(frozen=True, slots=True)
class ReviewItem:
    source: str
    line: int | None
    reason: str
    value: str


@dataclass(frozen=True, slots=True)
class FontPlan:
    game_root: Path
    content_root: Path
    selected_font: Path
    selected_sha256: str
    selected_size: int
    assets: tuple[FontAsset, ...]
    aliases: tuple[FontAlias, ...]
    references: tuple[FontReference, ...]
    reviews: tuple[ReviewItem, ...]
    mutations: tuple[ByteMutation, ...]
    coverage: FontCoverage


@dataclass(frozen=True, slots=True)
class _TextPatch:
    start: int
    end: int
    original: str
    replacement: str
    references: tuple[FontReference, ...]


@dataclass(frozen=True, slots=True)
class _HtmlAttribute:
    name: str
    value: str
    start: int
    end: int


@dataclass(frozen=True, slots=True)
class _HtmlTag:
    name: str
    attributes: tuple[_HtmlAttribute, ...]


@dataclass(frozen=True, slots=True)
class _HtmlRegion:
    kind: str
    start: int
    end: int
    attributes: tuple[_HtmlAttribute, ...]


def _asset_inventory(game_root: Path, files: Sequence[Path]) -> tuple[FontAsset, ...]:
    result: list[FontAsset] = []
    for path in files:
        if path.suffix.casefold() not in FONT_SUFFIXES:
            continue
        body = path.read_bytes()
        result.append(
            FontAsset(
                path=path,
                relative_path=path.relative_to(game_root).as_posix(),
                size=len(body),
                sha256=sha256_bytes(body),
            )
        )
    return tuple(sorted(result, key=lambda item: item.relative_path.casefold()))


def _html_attributes(body: str, *, offset: int) -> tuple[_HtmlAttribute, ...]:
    attributes: list[_HtmlAttribute] = []
    index = 0
    while index < len(body):
        while index < len(body) and (body[index].isspace() or body[index] == "/"):
            index += 1
        if index >= len(body):
            break
        name_start = index
        while index < len(body) and not body[index].isspace() and body[index] not in "=/>":
            index += 1
        if index == name_start:
            index += 1
            continue
        name = body[name_start:index].casefold()
        while index < len(body) and body[index].isspace():
            index += 1
        if index >= len(body) or body[index] != "=":
            continue
        index += 1
        while index < len(body) and body[index].isspace():
            index += 1
        if index >= len(body):
            break
        quote = body[index] if body[index] in {'"', "'"} else None
        if quote is not None:
            index += 1
            value_start = index
            end = body.find(quote, index)
            if end < 0:
                break
            index = end + 1
        else:
            value_start = index
            while index < len(body) and not body[index].isspace() and body[index] != ">":
                index += 1
            end = index
        attributes.append(
            _HtmlAttribute(
                name=name,
                value=body[value_start:end],
                start=offset + value_start,
                end=offset + end,
            )
        )
    return tuple(attributes)


def _html_structure(text: str) -> tuple[tuple[_HtmlTag, ...], tuple[_HtmlRegion, ...]]:
    tags: list[_HtmlTag] = []
    regions: list[_HtmlRegion] = []
    comments = tuple((match.start(), match.end()) for match in re.finditer(r"(?s)<!--.*?-->", text))
    cursor = 0
    while (match := _HTML_TAG.search(text, cursor)) is not None:
        comment = next(
            ((start, end) for start, end in comments if start <= match.start() < end),
            None,
        )
        if comment is not None:
            cursor = comment[1]
            continue
        if text.rfind("<!--", 0, match.start()) > text.rfind("-->", 0, match.start()):
            break
        cursor = match.end()
        if match.group("closing"):
            continue
        name = match.group("name").casefold()
        attributes = _html_attributes(match.group("body"), offset=match.start("body"))
        tags.append(_HtmlTag(name, attributes))
        if name not in {"style", "script"} or match.group("body").rstrip().endswith("/"):
            continue
        closing = re.search(rf"(?is)</\s*{re.escape(name)}\s*>", text[match.end() :])
        if closing is None:
            continue
        start = match.end()
        end = match.end() + closing.start()
        regions.append(_HtmlRegion(name, start, end, attributes))
        cursor = match.end() + closing.end()
    return tuple(tags), tuple(regions)


def _javascript_html_region(attributes: Sequence[_HtmlAttribute]) -> bool:
    values = {attribute.name: attribute.value.casefold().strip() for attribute in attributes}
    if "src" in values:
        return False
    script_type = values.get("type", "")
    return script_type in {
        "",
        "module",
        "text/javascript",
        "application/javascript",
        "text/ecmascript",
        "application/ecmascript",
    }


def _shift_scan_result(
    patches: Sequence[_TextPatch],
    reviews: Sequence[ReviewItem],
    *,
    offset: int,
    full_text: str,
) -> tuple[list[_TextPatch], list[ReviewItem]]:
    line_delta = full_text.count("\n", 0, offset)
    shifted_patches = [
        _TextPatch(
            patch.start + offset,
            patch.end + offset,
            patch.original,
            patch.replacement,
            tuple(replace(reference, line=reference.line + line_delta) for reference in patch.references),
        )
        for patch in patches
    ]
    shifted_reviews = [
        replace(review, line=None if review.line is None else review.line + line_delta) for review in reviews
    ]
    return shifted_patches, shifted_reviews


def _content_code_target(
    value: str,
    *,
    source: Path,
    content_root: Path,
    code_paths: Mapping[str, Path],
) -> Path | None:
    normalized = value.replace("\\", "/").split("?", 1)[0].split("#", 1)[0]
    if not normalized or normalized.startswith(("/", "//")) or ":" in normalized:
        return None
    try:
        relative = (
            (source.parent / Path(*PurePosixPath(normalized).parts))
            .resolve(strict=False)
            .relative_to(content_root)
        )
    except ValueError:
        return None
    return code_paths.get(relative.as_posix().casefold())


def _runtime_javascript_paths(
    *,
    game_root: Path,
    content_root: Path,
    files: Sequence[Path],
) -> tuple[frozenset[Path], list[ReviewItem]]:
    """从 NW.js 入口、plugins.js 活动项和静态加载调用确定会执行的 JS。"""

    code_paths = {
        path.relative_to(content_root).as_posix().casefold(): path.resolve(strict=True)
        for path in files
        if path.is_relative_to(content_root) and path.suffix.casefold() in {".js", ".mjs"}
    }
    active: set[Path] = set()
    reviews: list[ReviewItem] = []
    plugins = read_plugins(content_root)
    plugins_js = (content_root / "js" / "plugins.js").resolve(strict=True)
    active.add(plugins_js)
    for plugin in plugins:
        if not plugin.status:
            continue
        script = plugin_script_path(content_root, plugin.name)
        if script is None:
            reviews.append(
                ReviewItem(
                    f"js/plugins/{plugin.name}.js",
                    None,
                    "active_plugin_script_missing",
                    plugin.name,
                )
            )
            continue
        active.add(script.resolve(strict=True))

    package_path = content_root / "package.json"
    try:
        package = cast(object, json.loads(package_path.read_text(encoding="utf-8-sig")))
    except (OSError, UnicodeError, json.JSONDecodeError):
        package = None
    if isinstance(package, dict):
        main = cast(dict[object, object], package).get("main")
        if isinstance(main, str):
            main_value = main.replace("\\", "/").split("?", 1)[0].split("#", 1)[0]
            try:
                html = (content_root / Path(*PurePosixPath(main_value).parts)).resolve(strict=False)
                html.relative_to(content_root)
            except ValueError:
                html = None
            if html is not None and html.is_file() and html.suffix.casefold() in {".htm", ".html"}:
                try:
                    html_text = html.read_text(encoding="utf-8-sig")
                except (OSError, UnicodeError):
                    reviews.append(
                        ReviewItem(
                            html.relative_to(game_root).as_posix(),
                            None,
                            "runtime_html_unreadable",
                            "",
                        )
                    )
                else:
                    for match in _HTML_SCRIPT_SRC.finditer(html_text):
                        target = _content_code_target(
                            match.group("value"),
                            source=html,
                            content_root=content_root,
                            code_paths=code_paths,
                        )
                        if target is not None:
                            active.add(target)

    pending = sorted(active, key=lambda path: path.relative_to(content_root).as_posix().casefold())
    while pending:
        path = pending.pop(0)
        try:
            text, _bom = _decode_utf8(path.read_bytes())
        except (OSError, UnicodeError):
            continue
        relative = path.relative_to(content_root).as_posix()
        scan = scan_javascript(text)
        for literal in scan.literals:
            if literal.dynamic_template or not loader_call_on_line(scan.code, literal.line):
                continue
            for target_name in static_code_targets(literal.value, relative):
                target = code_paths.get(target_name.casefold())
                if target is None or target in active:
                    continue
                active.add(target)
                pending.append(target)
        pending.sort(key=lambda item: item.relative_to(content_root).as_posix().casefold())
    return frozenset(active), reviews


def _path_without_suffix(value: str) -> tuple[str, str]:
    marker_positions = [position for marker in ("?", "#") if (position := value.find(marker)) >= 0]
    if not marker_positions:
        return value, ""
    split = min(marker_positions)
    return value[:split], value[split:]


def _resolve_reference(
    value: str,
    *,
    source: Path,
    game_root: Path,
    content_root: Path,
    assets: Sequence[FontAsset],
) -> FontAsset | None:
    if not value or value != value.strip() or any(character in value for character in "\r\n\x00"):
        return None
    path_text, _ = _path_without_suffix(unquote(value))
    normalized = path_text.replace("\\", "/").removeprefix("./")
    if not normalized or ":" in normalized or normalized.startswith(("/", "//")):
        return None
    suffix = PurePosixPath(normalized).suffix.casefold()
    if suffix not in FONT_SUFFIXES:
        return None
    candidates: set[str] = set()
    relative = Path(*PurePosixPath(normalized).parts)
    for base in (source.parent, content_root, content_root / "fonts", game_root):
        candidate = (base / relative).resolve(strict=False)
        try:
            candidates.add(candidate.relative_to(game_root).as_posix().casefold())
        except ValueError:
            continue
    matches = [asset for asset in assets if asset.relative_path.casefold() in candidates]
    if not matches and "/" not in normalized:
        matches = [
            asset for asset in assets if Path(asset.relative_path).name.casefold() == normalized.casefold()
        ]
    return matches[0] if len(matches) == 1 else None


def _font_url_value(value: str) -> str:
    match = _CSS_URL.search(value)
    return match.group("value").strip() if match is not None else value


def _discover_aliases(
    *,
    game_root: Path,
    content_root: Path,
    files: Sequence[Path],
    assets: Sequence[FontAsset],
    runtime_javascript: frozenset[Path],
) -> tuple[tuple[FontAlias, ...], dict[str, FontAsset], list[ReviewItem]]:
    """从字体资产 stem、@font-face 和静态加载 API 建立别名到资产的证明图。"""

    facts: list[tuple[FontAlias, FontAsset]] = []
    reviews: list[ReviewItem] = []
    for asset in assets:
        stem = Path(asset.relative_path).stem
        if stem:
            facts.append(
                (
                    FontAlias(stem, asset.relative_path, "asset_stem", asset.relative_path, None),
                    asset,
                )
            )
    for path in files:
        suffix = path.suffix.casefold()
        if suffix not in {".css", ".htm", ".html", ".js", ".mjs"}:
            continue
        if suffix in {".js", ".mjs"} and path.resolve(strict=True) not in runtime_javascript:
            continue
        try:
            text, _bom = _decode_utf8(path.read_bytes())
        except UnicodeError:
            continue
        relative = path.relative_to(game_root).as_posix()
        css_fragments: list[tuple[str, int]] = []
        javascript_fragments: list[tuple[str, int]] = []
        if suffix == ".css":
            css_fragments.append((text, 0))
        elif suffix in {".htm", ".html"}:
            _tags, regions = _html_structure(text)
            css_fragments.extend(
                (text[region.start : region.end], region.start)
                for region in regions
                if region.kind == "style"
            )
            javascript_fragments.extend(
                (text[region.start : region.end], region.start)
                for region in regions
                if region.kind == "script" and _javascript_html_region(region.attributes)
            )
        else:
            javascript_fragments.append((text, 0))
        for fragment, offset in css_fragments:
            for block in _CSS_FONT_FACE.finditer(fragment):
                body = block.group("body")
                family_match = _CSS_FAMILY_DECLARATION.search(body)
                url_match = _CSS_URL.search(body)
                if family_match is None or url_match is None:
                    continue
                family = family_match.group("value").strip()
                url = url_match.group("value").strip()
                asset = _resolve_reference(
                    url,
                    source=path,
                    game_root=game_root,
                    content_root=content_root,
                    assets=assets,
                )
                if asset is None:
                    reviews.append(
                        ReviewItem(
                            relative,
                            _line(text, offset + block.start()),
                            "unresolved_font_face_asset",
                            url,
                        )
                    )
                elif family:
                    facts.append(
                        (
                            FontAlias(
                                family,
                                asset.relative_path,
                                "css_font_face",
                                relative,
                                _line(text, offset + block.start()),
                            ),
                            asset,
                        )
                    )
        for fragment, offset in javascript_fragments:
            literals = [
                literal
                for literal in scan_javascript(fragment).literals
                if literal.kind == "string"
                and literal.start is not None
                and literal.end is not None
                and literal.quote is not None
            ]
            for alias_literal, asset_literal in pairwise(literals):
                alias_end = cast(int, alias_literal.end)
                asset_start = cast(int, asset_literal.start)
                if fragment[alias_end:asset_start].strip() != ",":
                    continue
                alias_start = cast(int, alias_literal.start)
                before = fragment[max(0, alias_start - 120) : alias_start]
                if _JS_FONT_LOADER.search(before) is None:
                    continue
                url = _font_url_value(asset_literal.value)
                asset = _resolve_reference(
                    url,
                    source=path,
                    game_root=game_root,
                    content_root=content_root,
                    assets=assets,
                )
                if asset is None:
                    reviews.append(
                        ReviewItem(
                            relative,
                            _line(text, offset + alias_start),
                            "unresolved_font_loader_asset",
                            asset_literal.value,
                        )
                    )
                elif alias_literal.value:
                    facts.append(
                        (
                            FontAlias(
                                alias_literal.value,
                                asset.relative_path,
                                "javascript_font_loader",
                                relative,
                                _line(text, offset + alias_start),
                            ),
                            asset,
                        )
                    )
    by_value: dict[str, list[tuple[FontAlias, FontAsset]]] = {}
    for fact in facts:
        by_value.setdefault(fact[0].value.casefold(), []).append(fact)
    mapping: dict[str, FontAsset] = {}
    accepted: list[FontAlias] = []
    for normalized, candidates in sorted(by_value.items()):
        distinct = {candidate[1].relative_path.casefold() for candidate in candidates}
        if len(distinct) != 1:
            reviews.append(
                ReviewItem(
                    candidates[0][0].source,
                    candidates[0][0].line,
                    "font_alias_maps_to_multiple_assets",
                    candidates[0][0].value,
                )
            )
            continue
        mapping[normalized] = candidates[0][1]
        accepted.extend(candidate[0] for candidate in candidates)
    return tuple(accepted), mapping, reviews


def _resolve_token(
    value: str,
    *,
    source: Path,
    game_root: Path,
    content_root: Path,
    assets: Sequence[FontAsset],
    aliases: Mapping[str, FontAsset],
    selected_name: str,
    allow_alias: bool,
    allow_asset_path: bool = True,
) -> tuple[FontAsset, str, str] | None:
    asset = (
        _resolve_reference(
            value,
            source=source,
            game_root=game_root,
            content_root=content_root,
            assets=assets,
        )
        if allow_asset_path
        else None
    )
    if asset is not None:
        return asset, _new_value(value, selected_name), "asset_path"
    if not value or value != value.strip() or any(character in value for character in "\r\n\x00"):
        return None
    alias_asset = aliases.get(value.casefold()) if allow_alias else None
    if alias_asset is None:
        return None
    return alias_asset, Path(selected_name).stem, "font_alias"


def _is_font_semantic_name(value: str) -> bool:
    normalized = re.sub(r"[^a-z0-9]+", "", value.casefold())
    return "font" in normalized or "typeface" in normalized


def _property_name_before(text: str, start: int) -> str | None:
    prefix = text[max(0, start - 300) : start]
    match = re.search(
        r"(?s)(?:['\"](?P<quoted>[^'\"]+)['\"]|(?P<bare>[A-Za-z_$][A-Za-z0-9_$]*))\s*:\s*$", prefix
    )
    if match is None:
        return None
    return match.group("quoted") or match.group("bare")


def _javascript_alias_context(text: str, start: int) -> bool:
    property_name = _property_name_before(text, start)
    if property_name is not None and _is_font_semantic_name(property_name):
        return True
    prefix = text[max(0, start - 160) : start]
    return (
        _JS_FONT_CALL.search(prefix) is not None
        or re.search(r"(?i)(?:fontFace|fontFamily|fontName|typeface)\s*=\s*$", prefix) is not None
    )


def _new_value(value: str, selected_name: str) -> str:
    path_text, suffix = _path_without_suffix(value)
    slash = max(path_text.rfind("/"), path_text.rfind("\\"))
    return f"{path_text[: slash + 1]}{selected_name}{suffix}"


def _new_asset_relative(old_asset: FontAsset, selected_name: str) -> str:
    return (PurePosixPath(old_asset.relative_path).parent / selected_name).as_posix()


def _line(text: str, position: int) -> int:
    return text.count("\n", 0, position) + 1


def _reference(
    *,
    source_relative: str,
    source_text: str,
    position: int,
    context: str,
    asset: FontAsset,
    selected_name: str,
    old_value: str,
    new_value: str,
    nested_location: str | None = None,
) -> FontReference:
    return FontReference(
        source=source_relative,
        line=_line(source_text, position),
        context=context,
        old_asset=asset.relative_path,
        new_asset=_new_asset_relative(asset, selected_name),
        old_value=old_value,
        new_value=new_value,
        nested_location=nested_location,
    )


@dataclass(frozen=True, slots=True)
class _JsonStringToken:
    start: int
    end: int
    value: str
    path: tuple[str | int, ...]


class _JsonTokenParser:
    """保留重复 key 和原始字节位置，只解析 JSON string value 的自然路径。"""

    def __init__(self, text: str) -> None:
        self.text = text
        self.index = 0
        self.strings: list[_JsonStringToken] = []

    def parse(self) -> tuple[_JsonStringToken, ...]:
        self._skip_space()
        self._value(())
        self._skip_space()
        if self.index != len(self.text):
            raise ValueError("JSON 根值后存在多余内容")
        return tuple(self.strings)

    def _skip_space(self) -> None:
        while self.index < len(self.text) and self.text[self.index] in " \t\r\n":
            self.index += 1

    def _string(self) -> tuple[int, int, str]:
        if self.index >= len(self.text) or self.text[self.index] != '"':
            raise ValueError("JSON 当前位置不是 string")
        start = self.index
        self.index += 1
        escaped = False
        while self.index < len(self.text):
            character = self.text[self.index]
            self.index += 1
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                token = self.text[start : self.index]
                decoded = json.loads(token)
                if not isinstance(decoded, str):
                    raise TypeError("JSON string 解码结果异常")
                return start, self.index, decoded
        raise ValueError("JSON 字符串未闭合")

    def _value(self, path: tuple[str | int, ...]) -> None:
        self._skip_space()
        if self.index >= len(self.text):
            raise ValueError("JSON value 缺失")
        character = self.text[self.index]
        if character == '"':
            start, end, value = self._string()
            self.strings.append(_JsonStringToken(start, end, value, path))
            return
        if character == "{":
            self.index += 1
            self._skip_space()
            if self.index < len(self.text) and self.text[self.index] == "}":
                self.index += 1
                return
            while True:
                _start, _end, key = self._string()
                self._skip_space()
                if self.index >= len(self.text) or self.text[self.index] != ":":
                    raise ValueError("JSON object key 后缺少冒号")
                self.index += 1
                self._value((*path, key))
                self._skip_space()
                if self.index < len(self.text) and self.text[self.index] == "}":
                    self.index += 1
                    return
                if self.index >= len(self.text) or self.text[self.index] != ",":
                    raise ValueError("JSON object 条目后缺少逗号或右花括号")
                self.index += 1
                self._skip_space()
        if character == "[":
            self.index += 1
            self._skip_space()
            if self.index < len(self.text) and self.text[self.index] == "]":
                self.index += 1
                return
            item_index = 0
            while True:
                self._value((*path, item_index))
                item_index += 1
                self._skip_space()
                if self.index < len(self.text) and self.text[self.index] == "]":
                    self.index += 1
                    return
                if self.index >= len(self.text) or self.text[self.index] != ",":
                    raise ValueError("JSON array 条目后缺少逗号或右方括号")
                self.index += 1
                self._skip_space()
        try:
            _value, end = json.JSONDecoder().raw_decode(self.text, self.index)
        except json.JSONDecodeError as error:
            raise ValueError("JSON scalar 无效") from error
        self.index = end


def _common_edges(old: str, new: str) -> tuple[int, int]:
    prefix = 0
    while prefix < len(old) and prefix < len(new) and old[prefix] == new[prefix]:
        prefix += 1
    suffix = 0
    while (
        suffix < len(old) - prefix
        and suffix < len(new) - prefix
        and old[len(old) - suffix - 1] == new[len(new) - suffix - 1]
    ):
        suffix += 1
    return prefix, suffix


def _json_character_spans(token: str, expected: str) -> tuple[tuple[int, int], ...]:
    if len(token) < 2 or token[0] != '"' or token[-1] != '"':
        raise ValueError("JSON string token 缺少双引号")
    spans: list[tuple[int, int]] = []
    decoded: list[str] = []
    index = 1
    simple = {'"': '"', "\\": "\\", "/": "/", "b": "\b", "f": "\f", "n": "\n", "r": "\r", "t": "\t"}
    while index < len(token) - 1:
        start = index
        if token[index] != "\\":
            decoded.append(token[index])
            index += 1
            spans.append((start, index))
            continue
        if index + 1 >= len(token) - 1:
            raise ValueError("JSON string escape 不完整")
        escape = token[index + 1]
        if escape != "u":
            if escape not in simple:
                raise ValueError("JSON string escape 无效")
            decoded.append(simple[escape])
            index += 2
            spans.append((start, index))
            continue
        if index + 6 > len(token) - 1:
            raise ValueError("JSON unicode escape 不完整")
        high = int(token[index + 2 : index + 6], 16)
        index += 6
        if 0xD800 <= high <= 0xDBFF and token[index : index + 2] == "\\u" and index + 6 <= len(token) - 1:
            low = int(token[index + 2 : index + 6], 16)
            if 0xDC00 <= low <= 0xDFFF:
                decoded.append(chr(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)))
                index += 6
                spans.append((start, index))
                continue
        decoded.append(chr(high))
        spans.append((start, index))
    if "".join(decoded) != expected:
        raise ValueError("JSON string token 与已解码值不一致")
    return tuple(spans)


def _javascript_character_spans(token: str, expected: str) -> tuple[tuple[int, int], ...]:
    if len(token) < 2 or token[0] not in {'"', "'"} or token[-1] != token[0]:
        raise ValueError("JavaScript string token 引号无效")
    spans: list[tuple[int, int]] = []
    decoded: list[str] = []
    index = 1
    simple = {"b": "\b", "f": "\f", "n": "\n", "r": "\r", "t": "\t", "v": "\v", "0": "\0"}
    while index < len(token) - 1:
        start = index
        if token[index] != "\\":
            decoded.append(token[index])
            index += 1
            spans.append((start, index))
            continue
        index += 1
        if index >= len(token) - 1:
            raise ValueError("JavaScript string escape 不完整")
        escape = token[index]
        if escape in "\r\n":
            index += 1
            if escape == "\r" and index < len(token) - 1 and token[index] == "\n":
                index += 1
            continue
        if escape in simple:
            decoded.append(simple[escape])
            index += 1
        elif escape == "x" and index + 2 < len(token) - 1:
            decoded.append(chr(int(token[index + 1 : index + 3], 16)))
            index += 3
        elif escape == "u" and index + 1 < len(token) - 1 and token[index + 1] == "{":
            end = token.find("}", index + 2, len(token) - 1)
            if end < 0:
                raise ValueError("JavaScript unicode code point escape 不完整")
            decoded.append(chr(int(token[index + 2 : end], 16)))
            index = end + 1
        elif escape == "u" and index + 4 < len(token) - 1:
            decoded.append(chr(int(token[index + 1 : index + 5], 16)))
            index += 5
        else:
            decoded.append(escape)
            index += 1
        spans.append((start, index))
    if "".join(decoded) != expected:
        raise ValueError("JavaScript string token 与词法扫描值不一致")
    return tuple(spans)


def _encoded_fragment(value: str, *, syntax: str, quote: str) -> str:
    if syntax == "json":
        return json.dumps(value, ensure_ascii=False)[1:-1]
    return _javascript_string(value, quote)[1:-1]


def _preserving_value_patch(
    *,
    token_start: int,
    raw_token: str,
    old_value: str,
    new_value: str,
    syntax: str,
    references: tuple[FontReference, ...],
) -> _TextPatch:
    spans = (
        _json_character_spans(raw_token, old_value)
        if syntax == "json"
        else _javascript_character_spans(raw_token, old_value)
    )
    prefix, suffix = _common_edges(old_value, new_value)
    old_end = len(old_value) - suffix
    new_end = len(new_value) - suffix
    raw_start = spans[prefix][0] if prefix < len(spans) else len(raw_token) - 1
    raw_end = spans[old_end - 1][1] if old_end > prefix else raw_start
    quote = raw_token[0]
    replacement = _encoded_fragment(new_value[prefix:new_end], syntax=syntax, quote=quote)
    return _TextPatch(
        token_start + raw_start,
        token_start + raw_end,
        raw_token[raw_start:raw_end],
        replacement,
        references,
    )


def _nested_json_patches(
    decoded: str,
    *,
    source: Path,
    game_root: Path,
    content_root: Path,
    assets: Sequence[FontAsset],
    aliases: Mapping[str, FontAsset],
    selected_name: str,
    source_relative: str,
    source_text: str,
    position: int,
) -> tuple[_TextPatch, ...] | None:
    stripped = decoded.lstrip()
    if not stripped.startswith(("[", "{")):
        return None
    try:
        tokens = _JsonTokenParser(decoded).parse()
    except (TypeError, ValueError, json.JSONDecodeError):
        return None
    patches: list[_TextPatch] = []
    for token in tokens:
        font_context = any(isinstance(step, str) and _is_font_semantic_name(step) for step in token.path[-2:])
        resolved = _resolve_token(
            token.value,
            source=source,
            game_root=game_root,
            content_root=content_root,
            assets=assets,
            aliases=aliases,
            selected_name=selected_name,
            allow_alias=font_context,
            allow_asset_path=True,
        )
        if resolved is None:
            continue
        asset, replacement, token_kind = resolved
        location = "$" + "".join(f"[{step}]" if isinstance(step, int) else f".{step}" for step in token.path)
        reference = _reference(
            source_relative=source_relative,
            source_text=source_text,
            position=position,
            context=f"nested_json_{token_kind}",
            asset=asset,
            selected_name=selected_name,
            old_value=token.value,
            new_value=replacement,
            nested_location=location,
        )
        patches.append(
            _preserving_value_patch(
                token_start=token.start,
                raw_token=decoded[token.start : token.end],
                old_value=token.value,
                new_value=replacement,
                syntax="json",
                references=(reference,),
            )
        )
    return tuple(patches)


def _map_nested_patches_to_outer(
    *,
    raw_token: str,
    decoded: str,
    token_start: int,
    syntax: str,
    nested: Sequence[_TextPatch],
) -> tuple[_TextPatch, ...]:
    spans = (
        _json_character_spans(raw_token, decoded)
        if syntax == "json"
        else _javascript_character_spans(raw_token, decoded)
    )
    result: list[_TextPatch] = []
    for patch in nested:
        prefix, suffix = _common_edges(patch.original, patch.replacement)
        decoded_start = patch.start + prefix
        decoded_end = patch.end - suffix
        replacement_end = len(patch.replacement) - suffix
        raw_start = spans[decoded_start][0] if decoded_start < len(spans) else len(raw_token) - 1
        raw_end = spans[decoded_end - 1][1] if decoded_end > decoded_start else raw_start
        result.append(
            _TextPatch(
                token_start + raw_start,
                token_start + raw_end,
                raw_token[raw_start:raw_end],
                _encoded_fragment(
                    patch.replacement[prefix:replacement_end],
                    syntax=syntax,
                    quote=raw_token[0],
                ),
                patch.references,
            )
        )
    return tuple(result)


def _javascript_string(value: str, quote: str) -> str:
    escapes = {"\b": "\\b", "\f": "\\f", "\n": "\\n", "\r": "\\r", "\t": "\\t", "\v": "\\v"}
    result = [quote]
    for character in value:
        if character == "\\":
            result.append("\\\\")
        elif character == quote:
            result.append(f"\\{quote}")
        elif character in escapes:
            result.append(escapes[character])
        elif ord(character) < 0x20 or character in {"\u2028", "\u2029"}:
            result.append(f"\\u{ord(character):04X}")
        else:
            result.append(character)
    result.append(quote)
    return "".join(result)


def _scan_css(
    path: Path,
    text: str,
    *,
    game_root: Path,
    content_root: Path,
    assets: Sequence[FontAsset],
    aliases: Mapping[str, FontAsset],
    selected_name: str,
) -> tuple[list[_TextPatch], list[ReviewItem]]:
    relative = path.relative_to(game_root).as_posix()
    patches: list[_TextPatch] = []
    reviews: list[ReviewItem] = []
    selected_format = "opentype" if Path(selected_name).suffix.casefold() == ".otf" else "truetype"
    src_ranges = [
        (declaration.start("value"), declaration.end("value")) for declaration in _CSS_FONT_SRC.finditer(text)
    ]
    for match in _CSS_URL.finditer(text):
        value = match.group("value").strip()
        resolved = _resolve_token(
            value,
            source=path,
            game_root=game_root,
            content_root=content_root,
            assets=assets,
            aliases=aliases,
            selected_name=selected_name,
            allow_alias=False,
        )
        if resolved is None:
            if _FONT_WORD.search(value):
                reviews.append(
                    ReviewItem(relative, _line(text, match.start()), "unresolved_css_font_url", value)
                )
            continue
        asset, replacement, token_kind = resolved
        start = match.start("value") + (len(match.group("value")) - len(match.group("value").lstrip()))
        end = start + len(value)
        reference = _reference(
            source_relative=relative,
            source_text=text,
            position=start,
            context=f"css_url_{token_kind}",
            asset=asset,
            selected_name=selected_name,
            old_value=value,
            new_value=replacement,
        )
        patches.append(_TextPatch(start, end, value, replacement, (reference,)))
        for src_start, src_end in src_ranges:
            if not (src_start <= match.start() and match.end() <= src_end):
                continue
            comma = text.find(",", match.end(), src_end)
            bound_end = src_end if comma < 0 else comma
            tail = text[match.end() : bound_end]
            format_match = _CSS_FORMAT.match(tail)
            if format_match is not None:
                format_start = match.end() + format_match.start("value")
                format_end = match.end() + format_match.end("value")
                if text[format_start:format_end].casefold() != selected_format:
                    patches.append(
                        _TextPatch(
                            format_start,
                            format_end,
                            text[format_start:format_end],
                            selected_format,
                            (),
                        )
                    )
                break
            broad_format = _CSS_FORMAT_FUNCTION.match(tail)
            if broad_format is not None:
                format_start = match.end() + broad_format.start("function")
                format_end = match.end() + broad_format.end("function")
                patches.append(
                    _TextPatch(
                        format_start,
                        format_end,
                        text[format_start:format_end],
                        f'format("{selected_format}")',
                        (),
                    )
                )
                break
            if re.match(r"(?is)(?:\s|/\*.*?\*/)*format\s*\(", tail) is not None:
                reviews.append(
                    ReviewItem(relative, _line(text, match.end()), "unparsed_css_font_format", tail.strip())
                )
            break
    for declaration in _CSS_FONT_FAMILY.finditer(text):
        raw_value = declaration.group("value")
        value_start = declaration.start("value")
        cursor = 0
        for part in re.finditer(
            r"(?P<leading>\s*)(?P<quote>['\"]?)(?P<token>[^,'\"]+?)(?P=quote)(?P<trailing>\s*)(?:,|$)",
            raw_value,
        ):
            cursor = part.end()
            token = part.group("token").strip()
            resolved = _resolve_token(
                token,
                source=path,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=aliases,
                selected_name=selected_name,
                allow_alias=True,
            )
            if resolved is None:
                continue
            asset, replacement, token_kind = resolved
            token_offset = part.start("token") + (
                len(part.group("token")) - len(part.group("token").lstrip())
            )
            start = value_start + token_offset
            end = start + len(token)
            reference = _reference(
                source_relative=relative,
                source_text=text,
                position=start,
                context=f"css_font_family_{token_kind}",
                asset=asset,
                selected_name=selected_name,
                old_value=token,
                new_value=replacement,
            )
            patches.append(_TextPatch(start, end, token, replacement, (reference,)))
        if cursor < len(raw_value) and any(alias in raw_value.casefold() for alias in aliases):
            reviews.append(
                ReviewItem(
                    relative, _line(text, value_start), "unparsed_css_font_family_list", raw_value.strip()
                )
            )
    return patches, reviews


def _scan_json(
    path: Path,
    text: str,
    *,
    game_root: Path,
    content_root: Path,
    assets: Sequence[FontAsset],
    aliases: Mapping[str, FontAsset],
    selected_name: str,
) -> tuple[list[_TextPatch], list[ReviewItem]]:
    relative = path.relative_to(game_root).as_posix()
    patches: list[_TextPatch] = []
    reviews: list[ReviewItem] = []
    try:
        tokens = _JsonTokenParser(text).parse()
    except (TypeError, ValueError, json.JSONDecodeError):
        reviews = (
            [ReviewItem(relative, None, "invalid_json_with_possible_font_reference", "")]
            if _FONT_WORD.search(text) or any(alias in text.casefold() for alias in aliases)
            else []
        )
        return [], reviews
    for token in tokens:
        font_context = any(isinstance(step, str) and _is_font_semantic_name(step) for step in token.path[-2:])
        resolved = _resolve_token(
            token.value,
            source=path,
            game_root=game_root,
            content_root=content_root,
            assets=assets,
            aliases=aliases,
            selected_name=selected_name,
            allow_alias=font_context,
            allow_asset_path=True,
        )
        if resolved is not None:
            asset, replacement, token_kind = resolved
            reference = _reference(
                source_relative=relative,
                source_text=text,
                position=token.start,
                context=f"json_{token_kind}",
                asset=asset,
                selected_name=selected_name,
                old_value=token.value,
                new_value=replacement,
            )
            try:
                patches.append(
                    _preserving_value_patch(
                        token_start=token.start,
                        raw_token=text[token.start : token.end],
                        old_value=token.value,
                        new_value=replacement,
                        syntax="json",
                        references=(reference,),
                    )
                )
            except ValueError:
                reviews.append(
                    ReviewItem(
                        relative, _line(text, token.start), "unaddressable_json_font_value", token.value
                    )
                )
            continue
        nested = _nested_json_patches(
            token.value,
            source=path,
            game_root=game_root,
            content_root=content_root,
            assets=assets,
            aliases=aliases,
            selected_name=selected_name,
            source_relative=relative,
            source_text=text,
            position=token.start,
        )
        if nested:
            try:
                patches.extend(
                    _map_nested_patches_to_outer(
                        raw_token=text[token.start : token.end],
                        decoded=token.value,
                        token_start=token.start,
                        syntax="json",
                        nested=nested,
                    )
                )
            except ValueError:
                reviews.append(
                    ReviewItem(
                        relative,
                        _line(text, token.start),
                        "unaddressable_nested_json_font_value",
                        token.value,
                    )
                )
            continue
        if _FONT_WORD.search(token.value) or any(alias in token.value.casefold() for alias in aliases):
            reviews.append(
                ReviewItem(relative, _line(text, token.start), "unresolved_json_font_value", token.value)
            )
    return patches, reviews


def _scan_javascript(
    path: Path,
    text: str,
    *,
    game_root: Path,
    content_root: Path,
    assets: Sequence[FontAsset],
    aliases: Mapping[str, FontAsset],
    selected_name: str,
) -> tuple[list[_TextPatch], list[ReviewItem]]:
    relative = path.relative_to(game_root).as_posix()
    scan = scan_javascript(text)
    patches: list[_TextPatch] = []
    reviews: list[ReviewItem] = []
    for literal in scan.literals:
        if literal.kind != "string" or literal.start is None or literal.end is None or literal.quote is None:
            if _FONT_WORD.search(literal.value):
                reviews.append(
                    ReviewItem(
                        relative,
                        literal.line,
                        "dynamic_or_unaddressable_javascript_font_value",
                        literal.value,
                    )
                )
            continue
        cursor = literal.end
        while cursor < len(text) and text[cursor] in " \t\r\n":
            cursor += 1
        if cursor < len(text) and text[cursor] == ":":
            continue
        font_context = _javascript_alias_context(text, literal.start)
        resolved = _resolve_token(
            literal.value,
            source=path,
            game_root=game_root,
            content_root=content_root,
            assets=assets,
            aliases=aliases,
            selected_name=selected_name,
            allow_alias=font_context,
            allow_asset_path=True,
        )
        if resolved is not None:
            asset, replacement, token_kind = resolved
            reference = _reference(
                source_relative=relative,
                source_text=text,
                position=literal.start,
                context=f"javascript_{token_kind}",
                asset=asset,
                selected_name=selected_name,
                old_value=literal.value,
                new_value=replacement,
            )
            try:
                patches.append(
                    _preserving_value_patch(
                        token_start=literal.start,
                        raw_token=text[literal.start : literal.end],
                        old_value=literal.value,
                        new_value=replacement,
                        syntax="javascript",
                        references=(reference,),
                    )
                )
            except ValueError:
                reviews.append(
                    ReviewItem(relative, literal.line, "unaddressable_javascript_font_value", literal.value)
                )
            continue
        nested = _nested_json_patches(
            literal.value,
            source=path,
            game_root=game_root,
            content_root=content_root,
            assets=assets,
            aliases=aliases,
            selected_name=selected_name,
            source_relative=relative,
            source_text=text,
            position=literal.start,
        )
        if nested:
            try:
                patches.extend(
                    _map_nested_patches_to_outer(
                        raw_token=text[literal.start : literal.end],
                        decoded=literal.value,
                        token_start=literal.start,
                        syntax="javascript",
                        nested=nested,
                    )
                )
            except ValueError:
                reviews.append(
                    ReviewItem(
                        relative, literal.line, "unaddressable_nested_javascript_font_value", literal.value
                    )
                )
            continue
        if _FONT_WORD.search(literal.value) or any(alias in literal.value.casefold() for alias in aliases):
            reviews.append(
                ReviewItem(relative, literal.line, "unresolved_javascript_font_value", literal.value)
            )
    for warning in scan.warnings:
        if str(warning.get("kind", "")).startswith("unterminated_"):
            reviews.append(
                ReviewItem(
                    relative, int(warning.get("line", 1)), "javascript_lexical_structure_incomplete", ""
                )
            )
    return patches, reviews


def _scan_html(
    path: Path,
    text: str,
    *,
    game_root: Path,
    content_root: Path,
    assets: Sequence[FontAsset],
    aliases: Mapping[str, FontAsset],
    selected_name: str,
) -> tuple[list[_TextPatch], list[ReviewItem]]:
    """只在 HTML 属性和真实 style/script 内容中处理字体引用。"""

    relative = path.relative_to(game_root).as_posix()
    tags, regions = _html_structure(text)
    patches: list[_TextPatch] = []
    reviews: list[ReviewItem] = []
    for region in regions:
        fragment = text[region.start : region.end]
        if region.kind == "style":
            found_patches, found_reviews = _scan_css(
                path,
                fragment,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=aliases,
                selected_name=selected_name,
            )
        elif _javascript_html_region(region.attributes):
            found_patches, found_reviews = _scan_javascript(
                path,
                fragment,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=aliases,
                selected_name=selected_name,
            )
        else:
            continue
        shifted_patches, shifted_reviews = _shift_scan_result(
            found_patches,
            found_reviews,
            offset=region.start,
            full_text=text,
        )
        patches.extend(shifted_patches)
        reviews.extend(shifted_reviews)
    for tag in tags:
        values = {attribute.name: attribute.value.casefold().strip() for attribute in tag.attributes}
        for attribute in tag.attributes:
            if attribute.name == "style":
                found_patches, found_reviews = _scan_css(
                    path,
                    attribute.value,
                    game_root=game_root,
                    content_root=content_root,
                    assets=assets,
                    aliases=aliases,
                    selected_name=selected_name,
                )
                shifted_patches, shifted_reviews = _shift_scan_result(
                    found_patches,
                    found_reviews,
                    offset=attribute.start,
                    full_text=text,
                )
                patches.extend(shifted_patches)
                reviews.extend(shifted_reviews)
                continue
            font_context = _is_font_semantic_name(attribute.name) or (
                tag.name == "font" and attribute.name == "face"
            )
            asset_context = font_context or (
                tag.name == "link" and attribute.name == "href" and values.get("as") == "font"
            )
            if not font_context and not asset_context:
                continue
            resolved = _resolve_token(
                attribute.value,
                source=path,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=aliases,
                selected_name=selected_name,
                allow_alias=font_context,
                allow_asset_path=asset_context,
            )
            if resolved is None:
                if _FONT_WORD.search(attribute.value) or attribute.value.casefold() in aliases:
                    reviews.append(
                        ReviewItem(
                            relative,
                            _line(text, attribute.start),
                            "unresolved_html_font_attribute",
                            attribute.value,
                        )
                    )
                continue
            asset, replacement, token_kind = resolved
            reference = _reference(
                source_relative=relative,
                source_text=text,
                position=attribute.start,
                context=f"html_attribute_{token_kind}",
                asset=asset,
                selected_name=selected_name,
                old_value=attribute.value,
                new_value=replacement,
            )
            patches.append(
                _TextPatch(
                    attribute.start,
                    attribute.end,
                    attribute.value,
                    replacement,
                    (reference,),
                )
            )
    return patches, reviews


def _scan_generic_text(
    path: Path,
    text: str,
    *,
    game_root: Path,
    content_root: Path,
    assets: Sequence[FontAsset],
    aliases: Mapping[str, FontAsset],
    selected_name: str,
) -> tuple[list[_TextPatch], list[ReviewItem]]:
    """处理配置/XML/TXT 中作为完整值出现的已证明字体 token。"""

    relative = path.relative_to(game_root).as_posix()
    candidate_spans: dict[tuple[int, int], bool] = {}

    def add_candidate(start: int, end: int, *, allow_alias: bool) -> None:
        candidate_spans[(start, end)] = candidate_spans.get((start, end), False) or allow_alias

    for match in re.finditer(
        r"(?m)^(?P<key>[^=:\r\n]+?)[=:](?P<space>\s*)(?P<quote>['\"]?)(?P<value>[^'\"\r\n]+?)(?P=quote)\s*$",
        text,
    ):
        raw = match.group("value")
        leading = len(raw) - len(raw.lstrip())
        value = raw.strip()
        add_candidate(
            match.start("value") + leading,
            match.start("value") + leading + len(value),
            allow_alias=_is_font_semantic_name(match.group("key")),
        )
    for match in re.finditer(
        r"(?is)<(?P<key>[A-Za-z_][A-Za-z0-9_.:-]*)[^>]*>(?P<space>\s*)(?P<value>[^<>\r\n]+?)(?P<trailing>\s*)</(?P=key)\s*>",
        text,
    ):
        raw = match.group("value")
        leading = len(raw) - len(raw.lstrip())
        value = raw.strip()
        add_candidate(
            match.start("value") + leading,
            match.start("value") + leading + len(value),
            allow_alias=_is_font_semantic_name(match.group("key")),
        )
    for match in re.finditer(
        r"(?is)(?P<key>[A-Za-z_][A-Za-z0-9_.:-]*)\s*=\s*(?P<quote>['\"])(?P<value>[^'\"\r\n]+)(?P=quote)",
        text,
    ):
        add_candidate(
            match.start("value"),
            match.end("value"),
            allow_alias=_is_font_semantic_name(match.group("key")),
        )
    for match in re.finditer(r"(?P<quote>['\"])(?P<value>[^'\"\r\n]+)(?P=quote)", text):
        add_candidate(match.start("value"), match.end("value"), allow_alias=False)
    for match in re.finditer(r"(?m)^(?P<leading>\s*)(?P<value>\S(?:.*?\S)?)(?P<trailing>\s*)$", text):
        add_candidate(match.start("value"), match.end("value"), allow_alias=False)
    patches: list[_TextPatch] = []
    for (start, end), allow_alias in sorted(candidate_spans.items()):
        value = text[start:end]
        resolved = _resolve_token(
            value,
            source=path,
            game_root=game_root,
            content_root=content_root,
            assets=assets,
            aliases=aliases,
            selected_name=selected_name,
            allow_alias=allow_alias,
        )
        if resolved is None:
            continue
        asset, replacement, token_kind = resolved
        reference = _reference(
            source_relative=relative,
            source_text=text,
            position=start,
            context=f"config_complete_value_{token_kind}",
            asset=asset,
            selected_name=selected_name,
            old_value=value,
            new_value=replacement,
        )
        patches.append(_TextPatch(start, end, value, replacement, (reference,)))
    covered = [(patch.start, patch.end) for patch in patches]
    unresolved = False
    for match in _FONT_WORD.finditer(text):
        if not any(start <= match.start() and match.end() <= end for start, end in covered):
            unresolved = True
            break
    if not unresolved:
        for alias in aliases:
            for match in re.finditer(re.escape(alias), text, flags=re.IGNORECASE):
                if not any(start <= match.start() and match.end() <= end for start, end in covered):
                    unresolved = True
                    break
            if unresolved:
                break
    reviews = [ReviewItem(relative, None, "unclassified_or_partial_font_context", "")] if unresolved else []
    return patches, reviews


def _decode_utf8(body: bytes) -> tuple[str, bool]:
    bom = body.startswith(b"\xef\xbb\xbf")
    return body.decode("utf-8-sig" if bom else "utf-8"), bom


def _apply_text_patches(text: str, patches: Sequence[_TextPatch]) -> str:
    ordered = sorted(patches, key=lambda item: (item.start, item.end))
    previous_end = -1
    for patch in ordered:
        if patch.start < previous_end:
            raise ValueError("字体引用补丁范围重叠")
        if text[patch.start : patch.end] != patch.original:
            raise ValueError("字体引用补丁原文不一致")
        previous_end = patch.end
    result = text
    for patch in reversed(ordered):
        result = f"{result[: patch.start]}{patch.replacement}{result[patch.end :]}"
    return result


def build_font_plan(
    *,
    game_root: Path,
    content_root: Path,
    selected_font: Path,
    coverage_texts: Sequence[Path] = (),
) -> FontPlan:
    selected_body = selected_font.read_bytes()
    selected_name = selected_font.name
    if selected_font.suffix.casefold() not in {".otf", ".ttf"}:
        fail(str(selected_font), "替换字体必须是单个 OTF 或 TTF", "选择随包提供的未修改单字体文件")
    try:
        coverage = check_font_coverage(selected_font, coverage_texts)
    except (OSError, UnicodeError, ValueError) as error:
        fail(
            str(selected_font),
            f"无法校验字体字符覆盖（{type(error).__name__}）",
            "使用未损坏的单字体 OTF/TTF",
        )
    files = tuple(safe_walk_files(game_root))
    assets = _asset_inventory(game_root, files)
    runtime_javascript, runtime_reviews = _runtime_javascript_paths(
        game_root=game_root,
        content_root=content_root,
        files=files,
    )
    aliases, alias_mapping, alias_reviews = _discover_aliases(
        game_root=game_root,
        content_root=content_root,
        files=files,
        assets=assets,
        runtime_javascript=runtime_javascript,
    )
    references: list[FontReference] = []
    reviews: list[ReviewItem] = [*runtime_reviews, *alias_reviews]
    if coverage.missing_characters:
        reviews.append(
            ReviewItem(
                selected_font.name,
                None,
                "selected_font_missing_checked_characters",
                " ".join(f"U+{ord(character):04X}" for character in coverage.missing_characters),
            )
        )
    text_mutations: list[ByteMutation] = []
    referenced_assets: set[str] = set()
    target_asset_paths: set[str] = set()
    for path in files:
        if path.suffix.casefold() not in _SCANNED_TEXT_SUFFIXES:
            continue
        body = path.read_bytes()
        try:
            text, bom = _decode_utf8(body)
        except UnicodeError:
            if _FONT_WORD.search(body.decode("latin-1", errors="ignore")):
                reviews.append(
                    ReviewItem(
                        path.relative_to(game_root).as_posix(), None, "non_utf8_text_with_font_like_value", ""
                    )
                )
            continue
        suffix = path.suffix.casefold()
        patches: list[_TextPatch]
        found_reviews: list[ReviewItem]
        if suffix == ".json":
            patches, found_reviews = _scan_json(
                path,
                text,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=alias_mapping,
                selected_name=selected_name,
            )
        elif suffix in {".js", ".mjs"}:
            if path.resolve(strict=True) not in runtime_javascript:
                if _FONT_WORD.search(text) or any(alias in text.casefold() for alias in alias_mapping):
                    reviews.append(
                        ReviewItem(
                            path.relative_to(game_root).as_posix(),
                            None,
                            "inactive_or_unproven_javascript_font_consumer",
                            "",
                        )
                    )
                continue
            patches, found_reviews = _scan_javascript(
                path,
                text,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=alias_mapping,
                selected_name=selected_name,
            )
        elif suffix == ".css":
            patches, found_reviews = _scan_css(
                path,
                text,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=alias_mapping,
                selected_name=selected_name,
            )
        elif suffix in {".htm", ".html"}:
            patches, found_reviews = _scan_html(
                path,
                text,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=alias_mapping,
                selected_name=selected_name,
            )
        else:
            patches, found_reviews = _scan_generic_text(
                path,
                text,
                game_root=game_root,
                content_root=content_root,
                assets=assets,
                aliases=alias_mapping,
                selected_name=selected_name,
            )
        reviews.extend(found_reviews)
        if not patches:
            continue
        updated = _apply_text_patches(text, patches)
        encoded = (b"\xef\xbb\xbf" if bom else b"") + updated.encode("utf-8")
        if encoded != body:
            text_mutations.append(ByteMutation(path.relative_to(game_root).as_posix(), body, encoded))
        for patch in patches:
            references.extend(patch.references)
            for reference in patch.references:
                referenced_assets.add(reference.old_asset.casefold())
                target_asset_paths.add(reference.new_asset)

    selected_sha = sha256_bytes(selected_body)
    asset_by_relative = {asset.relative_path.casefold(): asset for asset in assets}
    asset_mutations: list[ByteMutation] = []
    for relative in sorted(target_asset_paths, key=str.casefold):
        current = asset_by_relative.get(relative.casefold())
        if current is not None:
            if current.sha256 != selected_sha:
                fail(
                    relative,
                    "替换字体目标已存在但字节与选择字体不同",
                    "更换字体文件名，或先调查并恢复该冲突资源；工具不会覆盖未知字体",
                )
            continue
        asset_mutations.append(ByteMutation(relative, None, selected_body))
    for asset in assets:
        if asset.relative_path.casefold() not in referenced_assets and asset.sha256 != selected_sha:
            reviews.append(
                ReviewItem(
                    asset.relative_path,
                    None,
                    "unreferenced_font_asset_or_dynamic_consumer",
                    asset.relative_path,
                )
            )
    mutations = (*asset_mutations, *text_mutations)
    return FontPlan(
        game_root=game_root,
        content_root=content_root,
        selected_font=selected_font,
        selected_sha256=selected_sha,
        selected_size=len(selected_body),
        assets=assets,
        aliases=aliases,
        references=tuple(references),
        reviews=tuple(reviews),
        mutations=mutations,
        coverage=coverage,
    )
