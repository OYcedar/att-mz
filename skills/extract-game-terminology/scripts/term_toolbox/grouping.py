"""把 Manual 条目按公开自然范围整理为 Formic 单元。"""

from __future__ import annotations

import re
from collections.abc import Sequence
from dataclasses import dataclass

from att_skill_tools import ManualEntry, fail

FORMIC_TARGET_RENDERED_CHARACTERS = 24_000
_GENERIC_LINE = re.compile(r"line[1-9][0-9]*\Z")
_MAP_FILE = re.compile(r"Map[0-9]+\.json\Z")
_EVENT = re.compile(r"event[1-9][0-9]*\Z")
_TROOP = re.compile(r"troop[1-9][0-9]*\Z")
_NATURAL_NUMBER = re.compile(r"[1-9][0-9]*\Z")
_PLUGIN = re.compile(r"plugin[1-9][0-9]*\Z")


@dataclass(frozen=True, slots=True)
class FormicUnit:
    """不拆 Manual entry 的 Formic 输入单元。"""

    title: str
    scopes: tuple[str, ...]
    entries: tuple[ManualEntry, ...]
    source_characters: int


def natural_scope(readable_id: str) -> tuple[str, str, bool]:
    """返回可读自然范围、来源文件和是否可与相邻 Generic Group 装箱。"""

    parts = readable_id.split(":")
    source = parts[0]
    if len(parts) >= 2 and _GENERIC_LINE.fullmatch(parts[1]):
        return f"{source}:{parts[1]}", source, True
    if len(parts) >= 2:
        position = parts[1]
        if (_MAP_FILE.fullmatch(source) and _EVENT.fullmatch(position)) or (
            source == "CommonEvents.json"
            and (_EVENT.fullmatch(position) or _NATURAL_NUMBER.fullmatch(position))
        ):
            return f"{source}:{position}", source, False
        if source == "Troops.json" and (_TROOP.fullmatch(position) or _NATURAL_NUMBER.fullmatch(position)):
            return f"{source}:{position}", source, False
        if source == "plugins.js" and _PLUGIN.fullmatch(position):
            return f"{source}:{position}", source, False
    return source, source, False


def _source_characters(entry: ManualEntry) -> int:
    return sum(len(line) for line in entry.source) + max(0, len(entry.source) - 1)


def _fenced(lines: Sequence[str]) -> str:
    text = "\n".join(lines)
    longest = max((len(match.group(0)) for match in re.finditer(r"`+", text)), default=0)
    fence = "`" * max(3, longest + 1)
    return f"{fence}text\n{text}\n{fence}"


def _entry_block(entry: ManualEntry) -> str:
    return f"\n### {entry.readable_id}\n\n{_fenced(entry.source)}"


def _scope_body(scope: str, entries: Sequence[ManualEntry]) -> str:
    return f"\n## {scope}" + "".join(_entry_block(entry) for entry in entries)


def render_formic_unit(unit: FormicUnit) -> str:
    """生成 prepare 与 review 共用的精确 Formic 输入正文。"""

    scoped: dict[str, list[ManualEntry]] = {}
    order: list[str] = []
    for entry in unit.entries:
        scope, _, _ = natural_scope(entry.readable_id)
        if scope not in scoped:
            order.append(scope)
            scoped[scope] = []
        scoped[scope].append(entry)
    return f"# {unit.title}" + "".join(_scope_body(scope, scoped[scope]) for scope in order) + "\n"


def _scope_chunks(
    scope: str,
    entries: Sequence[ManualEntry],
    *,
    target_characters: int,
) -> list[tuple[tuple[ManualEntry, ...], int]]:
    chunks: list[tuple[tuple[ManualEntry, ...], int]] = []
    current: list[ManualEntry] = []
    current_source = 0
    reserve_title = f"{scope}（第 00000000000000000000 段）"
    rendered = len(f"# {reserve_title}\n## {scope}\n")
    for entry in entries:
        source_characters = _source_characters(entry)
        entry_characters = len(_entry_block(entry))
        if current and rendered + entry_characters > target_characters:
            chunks.append((tuple(current), current_source))
            current = []
            current_source = 0
            rendered = len(f"# {reserve_title}\n## {scope}\n")
        current.append(entry)
        current_source += source_characters
        rendered += entry_characters
    if current:
        chunks.append((tuple(current), current_source))
    if not chunks:
        fail(scope, "自然范围没有 Manual entry", "重新运行当前版本的 Manual export")
    return chunks


def _unit_title(scopes: Sequence[str]) -> str:
    if len(scopes) == 1:
        return scopes[0]
    source = scopes[0].split(":", 1)[0]
    first = scopes[0].split(":", 1)[1]
    last = scopes[-1].split(":", 1)[1]
    return f"{source}:{first}–{last}"


def build_formic_units(
    entries: Sequence[ManualEntry],
    *,
    target_characters: int = FORMIC_TARGET_RENDERED_CHARACTERS,
) -> list[FormicUnit]:
    """按公开自然范围分组，只在完整 Manual entry 边界限制实际 Markdown 体积。"""

    if target_characters <= 0:
        fail("Formic 单元目标", "目标字符数不是正整数", "使用正整数目标")
    scope_order: list[str] = []
    scoped: dict[str, list[ManualEntry]] = {}
    scope_facts: dict[str, tuple[str, bool]] = {}
    for entry in entries:
        scope, source, pack_generic = natural_scope(entry.readable_id)
        if scope not in scoped:
            scope_order.append(scope)
            scoped[scope] = []
            scope_facts[scope] = (source, pack_generic)
        scoped[scope].append(entry)

    atomic: list[tuple[str, str, bool, tuple[ManualEntry, ...], int, int, int]] = []
    for scope in scope_order:
        source, pack_generic = scope_facts[scope]
        chunks = _scope_chunks(scope, scoped[scope], target_characters=target_characters)
        for segment, (chunk, characters) in enumerate(chunks, start=1):
            atomic.append((scope, source, pack_generic, chunk, characters, segment, len(chunks)))

    units: list[FormicUnit] = []
    pending_scopes: list[str] = []
    pending_entries: list[ManualEntry] = []
    pending_source_characters = 0
    pending_body_characters = 0
    pending_source: str | None = None

    def flush_pending() -> None:
        nonlocal pending_body_characters, pending_source, pending_source_characters
        if not pending_entries:
            return
        units.append(
            FormicUnit(
                title=_unit_title(pending_scopes),
                scopes=tuple(pending_scopes),
                entries=tuple(pending_entries),
                source_characters=pending_source_characters,
            )
        )
        pending_scopes.clear()
        pending_entries.clear()
        pending_source_characters = 0
        pending_body_characters = 0
        pending_source = None

    for scope, source, pack_generic, chunk, characters, segment, segment_count in atomic:
        if not pack_generic or segment_count > 1:
            flush_pending()
            title = scope if segment_count == 1 else f"{scope}（第 {segment} 段）"
            units.append(
                FormicUnit(
                    title=title,
                    scopes=(scope,),
                    entries=chunk,
                    source_characters=characters,
                )
            )
            continue
        prospective_title = _unit_title([*pending_scopes, scope])
        scope_body_characters = len(_scope_body(scope, chunk))
        prospective_rendered = (
            len(f"# {prospective_title}") + pending_body_characters + scope_body_characters + 1
        )
        can_pack = (
            bool(pending_entries) and pending_source == source and prospective_rendered <= target_characters
        )
        if not can_pack:
            flush_pending()
        pending_source = source
        pending_scopes.append(scope)
        pending_entries.extend(chunk)
        pending_source_characters += characters
        pending_body_characters += scope_body_characters
    flush_pending()
    return units
