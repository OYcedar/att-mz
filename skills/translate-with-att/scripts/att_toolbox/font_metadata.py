"""单字体 sfnt Unicode cmap 的只读元数据与字符覆盖。"""

from __future__ import annotations

import struct
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

_BASELINE_CHARACTERS = "中文汉化测试，。！？：；、“”‘’（）【】《》…—·0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"


@dataclass(frozen=True, slots=True)
class FontCoverage:
    checked_characters: str
    missing_characters: str
    glyph_count: int


def _u16(data: bytes, offset: int) -> int:
    if offset < 0 or offset + 2 > len(data):
        raise ValueError("字体表越界")
    return struct.unpack_from(">H", data, offset)[0]


def _u32(data: bytes, offset: int) -> int:
    if offset < 0 or offset + 4 > len(data):
        raise ValueError("字体表越界")
    return struct.unpack_from(">I", data, offset)[0]


def _parse_cmap_format_4(data: bytes, offset: int) -> set[int]:
    length = _u16(data, offset + 2)
    end = offset + length
    if length < 16 or end > len(data):
        raise ValueError("cmap format 4 长度无效")
    segment_count = _u16(data, offset + 6) // 2
    end_codes = offset + 14
    start_codes = end_codes + 2 * segment_count + 2
    deltas = start_codes + 2 * segment_count
    ranges = deltas + 2 * segment_count
    if ranges + 2 * segment_count > end:
        raise ValueError("cmap format 4 分段表不完整")
    result: set[int] = set()
    for index in range(segment_count):
        segment_end = _u16(data, end_codes + 2 * index)
        segment_start = _u16(data, start_codes + 2 * index)
        delta = _u16(data, deltas + 2 * index)
        range_offset_position = ranges + 2 * index
        range_offset = _u16(data, range_offset_position)
        if segment_start > segment_end:
            raise ValueError("cmap format 4 分段范围反向")
        for codepoint in range(segment_start, segment_end + 1):
            if codepoint == 0xFFFF:
                continue
            if range_offset == 0:
                glyph = (codepoint + delta) & 0xFFFF
            else:
                glyph_position = range_offset_position + range_offset + 2 * (codepoint - segment_start)
                if glyph_position + 2 > end:
                    raise ValueError("cmap format 4 glyphIdArray 越界")
                glyph = _u16(data, glyph_position)
                if glyph:
                    glyph = (glyph + delta) & 0xFFFF
            if glyph:
                result.add(codepoint)
    return result


def _parse_cmap_format_12_or_13(data: bytes, offset: int, format_number: int) -> set[int]:
    length = _u32(data, offset + 4)
    end = offset + length
    groups = _u32(data, offset + 12)
    if length < 16 or offset + 16 + groups * 12 > end or end > len(data):
        raise ValueError(f"cmap format {format_number} 长度无效")
    result: set[int] = set()
    for index in range(groups):
        position = offset + 16 + index * 12
        start = _u32(data, position)
        finish = _u32(data, position + 4)
        glyph = _u32(data, position + 8)
        if start > finish or finish > 0x10FFFF:
            raise ValueError(f"cmap format {format_number} 分组范围无效")
        if format_number == 13 and glyph == 0:
            continue
        if format_number == 12 and glyph == 0:
            start += 1
        result.update(range(start, finish + 1))
    return result


def font_codepoints(path: Path) -> set[int]:
    """读取单字体 sfnt 的 Unicode cmap；不依赖 fontTools。"""

    data = path.read_bytes()
    if data[:4] == b"ttcf":
        raise ValueError("暂不接受字体集合 TTC/OTC；请选择单个未修改字体文件")
    if len(data) < 12 or data[:4] not in {b"\x00\x01\x00\x00", b"OTTO", b"true", b"typ1"}:
        raise ValueError("不是可识别的单字体 sfnt 文件")
    table_count = _u16(data, 4)
    cmap_offset: int | None = None
    cmap_length: int | None = None
    for index in range(table_count):
        position = 12 + index * 16
        if position + 16 > len(data):
            raise ValueError("sfnt table directory 不完整")
        if data[position : position + 4] == b"cmap":
            cmap_offset = _u32(data, position + 8)
            cmap_length = _u32(data, position + 12)
            break
    if cmap_offset is None or cmap_length is None or cmap_offset + cmap_length > len(data):
        raise ValueError("字体缺少有效 cmap 表")
    encoding_count = _u16(data, cmap_offset + 2)
    subtables: set[int] = set()
    for index in range(encoding_count):
        position = cmap_offset + 4 + index * 8
        platform = _u16(data, position)
        encoding = _u16(data, position + 2)
        subtable = cmap_offset + _u32(data, position + 4)
        if platform == 0 or (platform == 3 and encoding in {1, 10}):
            subtables.add(subtable)
    result: set[int] = set()
    for subtable in subtables:
        format_number = _u16(data, subtable)
        if format_number == 4:
            result.update(_parse_cmap_format_4(data, subtable))
        elif format_number in {12, 13}:
            result.update(_parse_cmap_format_12_or_13(data, subtable, format_number))
    if not result:
        raise ValueError("字体没有可读取的 Unicode cmap 4/12/13 子表")
    return result


def check_font_coverage(font: Path, extra_texts: Sequence[Path] = ()) -> FontCoverage:
    characters = set(_BASELINE_CHARACTERS)
    for path in extra_texts:
        characters.update(path.read_text(encoding="utf-8-sig"))
    characters = {
        character
        for character in characters
        if character not in {"\ufeff", "\ufffe", "\xffff"} and not character.isspace()
    }
    codepoints = font_codepoints(font)
    checked = "".join(sorted(characters, key=ord))
    missing = "".join(character for character in checked if ord(character) not in codepoints)
    return FontCoverage(
        checked_characters=checked,
        missing_characters=missing,
        glyph_count=len(codepoints),
    )
