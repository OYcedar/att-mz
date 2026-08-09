"""把 Manual 条目保持自然 Scope，并按来源装入 Formic 单元。"""

from __future__ import annotations

import re
from collections.abc import Sequence
from dataclasses import dataclass

from att_skill_tools import JsonValue, ManualEntry, fail

FORMIC_TARGET_RENDERED_CHARACTERS = 24_000
_GENERIC_LINE = re.compile(r"line[1-9][0-9]*\Z")
_MAP_FILE = re.compile(r"Map[0-9]+\.json\Z")
_EVENT = re.compile(r"event[1-9][0-9]*\Z")
_TROOP = re.compile(r"troop[1-9][0-9]*\Z")
_NATURAL_NUMBER = re.compile(r"[1-9][0-9]*\Z")
_PLUGIN = re.compile(r"plugin[1-9][0-9]*\Z")
_INVALID_FILENAME = re.compile(r'[<>:"/\\|?*\x00-\x1f]')


@dataclass(frozen=True, slots=True)
class FormicScope:
    """一个不可拆分的 Manual 自然 Scope 文件。"""

    title: str
    source: str
    file_name: str
    entries: tuple[ManualEntry, ...]
    source_characters: int


@dataclass(frozen=True, slots=True)
class FormicUnit:
    """同一来源中一个或多个相邻完整 Scope 组成的 Formic 单元。"""

    title: str
    scopes: tuple[FormicScope, ...]
    entries: tuple[ManualEntry, ...]
    source_characters: int


def natural_scope(readable_id: str) -> tuple[str, str]:
    """返回可读自然 Scope 与来源；Scope 在后续装箱中始终保持完整。"""

    parts = readable_id.split(":")
    source = parts[0]
    if len(parts) >= 2 and _GENERIC_LINE.fullmatch(parts[1]):
        return f"{source}:{parts[1]}", source
    if len(parts) >= 2:
        position = parts[1]
        if (_MAP_FILE.fullmatch(source) and _EVENT.fullmatch(position)) or (
            source == "CommonEvents.json"
            and (_EVENT.fullmatch(position) or _NATURAL_NUMBER.fullmatch(position))
        ):
            return f"{source}:{position}", source
        if source == "Troops.json" and (_TROOP.fullmatch(position) or _NATURAL_NUMBER.fullmatch(position)):
            return f"{source}:{position}", source
        if source == "plugins.js" and _PLUGIN.fullmatch(position):
            return f"{source}:{position}", source
    return source, source


def _safe_name(value: str) -> str:
    cleaned = _INVALID_FILENAME.sub("_", value).rstrip(" .")
    return (cleaned or "未分类来源")[:80].rstrip(" .") or "未分类来源"


def _source_characters(entry: ManualEntry) -> int:
    return sum(len(line) for line in entry.source) + max(0, len(entry.source) - 1)


def _fenced(lines: Sequence[str]) -> str:
    text = "\n".join(lines)
    longest = max((len(match.group(0)) for match in re.finditer(r"`+", text)), default=0)
    fence = "`" * max(3, longest + 1)
    return f"{fence}text\n{text}\n{fence}"


def _entry_block(entry: ManualEntry) -> str:
    return f"\n## {entry.readable_id}\n\n{_fenced(entry.source)}"


def render_formic_scope(scope: FormicScope) -> str:
    """生成一个自然 Scope 文件的完整内容。"""

    return f"# {scope.title}" + "".join(_entry_block(entry) for entry in scope.entries) + "\n"


def render_formic_unit(unit: FormicUnit) -> str:
    """按 Formic 文件清单模式生成该单元实际追加到 prompt 的分片正文。"""

    rendered: list[str] = []
    for index, scope in enumerate(unit.scopes):
        if index:
            rendered.append("\n")
        rendered.append(f"## 文件 {scope.file_name}\n")
        rendered.append(render_formic_scope(scope).rstrip("\n"))
        rendered.append("\n")
    return "".join(rendered)


def _scope_rendered_characters(scope: FormicScope) -> int:
    return len(f"## 文件 {scope.file_name}\n") + len(render_formic_scope(scope).rstrip("\n")) + 1


def _unit_title(scopes: Sequence[FormicScope]) -> str:
    if len(scopes) == 1:
        return scopes[0].title
    first = scopes[0].title
    last = scopes[-1].title
    if ":" not in first or ":" not in last:
        return first
    source, first_position = first.split(":", 1)
    _, last_position = last.split(":", 1)
    return f"{source}:{first_position}–{last_position}"


def _scope_files(entries: Sequence[ManualEntry]) -> list[FormicScope]:
    grouped: list[tuple[str, str, list[ManualEntry]]] = []
    closed: set[str] = set()
    for entry in entries:
        title, source = natural_scope(entry.readable_id)
        if grouped and grouped[-1][0] == title:
            grouped[-1][2].append(entry)
            continue
        if title in closed:
            fail(
                title,
                "同一自然 Scope 在 Manual 中不连续",
                "重新运行当前 ATT manual export，不要手工重排条目",
            )
        if grouped:
            closed.add(grouped[-1][0])
        grouped.append((title, source, [entry]))
    result: list[FormicScope] = []
    for number, (title, source, scope_entries) in enumerate(grouped, start=1):
        result.append(
            FormicScope(
                title=title,
                source=source,
                file_name=f"{number:06d}-{_safe_name(title)}.md",
                entries=tuple(scope_entries),
                source_characters=sum(_source_characters(entry) for entry in scope_entries),
            )
        )
    return result


def _make_unit(scopes: Sequence[FormicScope]) -> FormicUnit:
    entries = tuple(entry for scope in scopes for entry in scope.entries)
    return FormicUnit(
        title=_unit_title(scopes),
        scopes=tuple(scopes),
        entries=entries,
        source_characters=sum(scope.source_characters for scope in scopes),
    )


def build_formic_units(
    entries: Sequence[ManualEntry],
    *,
    target_characters: int = FORMIC_TARGET_RENDERED_CHARACTERS,
) -> list[FormicUnit]:
    """保持 Scope 完整，只把同一来源的相邻 Scope 装到约 24k 的单元。"""

    if target_characters <= 0:
        fail("Formic 单元目标", "目标字符数不是正整数", "使用正整数目标")
    scopes = _scope_files(entries)
    units: list[FormicUnit] = []
    pending: list[FormicScope] = []
    pending_rendered = 0

    def flush() -> None:
        nonlocal pending_rendered
        if pending:
            units.append(_make_unit(pending))
            pending.clear()
            pending_rendered = 0

    for scope in scopes:
        if pending and pending[-1].source != scope.source:
            flush()
        scope_rendered = _scope_rendered_characters(scope)
        prospective = pending_rendered + (1 if pending else 0) + scope_rendered
        if pending and prospective > target_characters:
            flush()
        pending.append(scope)
        pending_rendered += (1 if len(pending) > 1 else 0) + scope_rendered
    flush()
    return units


def formic_packing_evidence(
    units: Sequence[FormicUnit],
    *,
    target_characters: int,
) -> dict[str, JsonValue]:
    """给高单元数作业提供可读边界事实。"""

    scopes = [scope for unit in units for scope in unit.scopes]
    oversized: list[JsonValue] = []
    for unit_number, unit in enumerate(units, start=1):
        for scope in unit.scopes:
            rendered_characters = _scope_rendered_characters(scope)
            if rendered_characters > target_characters:
                oversized.append(
                    {
                        "unit": unit_number,
                        "scope": scope.title,
                        "source": scope.source,
                        "file": scope.file_name,
                        "rendered_characters": rendered_characters,
                    }
                )
    source_runs = 0
    previous: str | None = None
    for scope in scopes:
        if scope.source != previous:
            source_runs += 1
            previous = scope.source
    return {
        "units": len(units),
        "scopes": len(scopes),
        "source_runs": source_runs,
        "oversized_scopes": len(oversized),
        "oversized_scope_details": oversized,
        "total_rendered_characters": sum(len(render_formic_unit(unit)) for unit in units),
        "maximum_rendered_characters": max((len(render_formic_unit(unit)) for unit in units), default=0),
        "target_rendered_characters": target_characters,
    }
