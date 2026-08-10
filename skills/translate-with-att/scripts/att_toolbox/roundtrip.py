"""供审核后 Generic 转换使用的精确 JavaScript 与纯文本往返助手。"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass

from att_skill_tools import fail

from att_toolbox.js import scan_javascript


@dataclass(frozen=True, slots=True)
class JavaScriptReplacementResult:
    text: str
    line: int
    original: str
    replacement: str


@dataclass(frozen=True, slots=True)
class PlainTextLine:
    line_number: int
    text: str
    ending: str


@dataclass(frozen=True, slots=True)
class PlainTextReplacement:
    line_number: int
    source: str
    translation: str


def _javascript_string(value: str, quote: str) -> str:
    escaped: list[str] = [quote]
    escapes = {
        "\b": "\\b",
        "\f": "\\f",
        "\n": "\\n",
        "\r": "\\r",
        "\t": "\\t",
        "\v": "\\v",
        "\\": "\\\\",
        quote: f"\\{quote}",
    }
    for character in value:
        replacement = escapes.get(character)
        if replacement is not None:
            escaped.append(replacement)
        elif ord(character) < 0x20 or character in {"\u2028", "\u2029"}:
            escaped.append(f"\\u{ord(character):04X}")
        else:
            escaped.append(character)
    escaped.append(quote)
    return "".join(escaped)


def replace_reviewed_javascript_literal(
    text: str,
    *,
    line: int,
    source: str,
    translation: str,
    reviewed: bool,
) -> JavaScriptReplacementResult:
    """只替换审核决定指定行上唯一一个完全相等的普通字符串字面量。"""

    if not reviewed:
        fail("JavaScript 字面量替换", "没有明确的审核决定", "先确认 Generic 七项证据和精确写回位置")
    if line <= 0:
        fail("JavaScript 字面量替换", "line 不是正整数", "使用 trace 证据中的自然行号")
    if not source:
        fail("JavaScript 字面量替换", "source 为空", "填写审核时看到的完整原字面量")
    if not translation:
        fail("JavaScript 字面量替换", "translation 为空", "填写经过审核的非空译文")
    scan = scan_javascript(text)
    if any(str(warning.get("kind", "")).startswith("unterminated_") for warning in scan.warnings):
        fail("JavaScript 字面量替换", "源码存在未闭合词法结构", "先修正或确认完整 JavaScript 源码")
    matches = [
        literal
        for literal in scan.literals
        if literal.kind == "string"
        and literal.line == line
        and literal.value == source
        and literal.start is not None
        and literal.end is not None
        and literal.quote in {"'", '"'}
    ]
    if len(matches) != 1:
        fail(
            "JavaScript 字面量替换",
            f"指定行与原文应唯一命中 1 个普通字符串字面量，实际命中 {len(matches)} 个",
            "重新 trace 并用精确自然行号和完整原字面量审核；不要做全局替换",
        )
    literal = matches[0]
    start = literal.start
    end = literal.end
    quote = literal.quote
    if start is None or end is None or quote is None:
        fail("JavaScript 字面量替换", "无法取得精确字面量边界", "报告当前随包工具实现错误")
    replacement = _javascript_string(translation, quote)
    return JavaScriptReplacementResult(
        text=f"{text[:start]}{replacement}{text[end:]}",
        line=line,
        original=source,
        replacement=translation,
    )


def plain_text_lines(text: str) -> tuple[PlainTextLine, ...]:
    """按自然行列出纯文本，保留每行原始 CRLF/LF/CR 结尾。"""

    result: list[PlainTextLine] = []
    cursor = 0
    line_number = 1
    while cursor < len(text):
        end = cursor
        while end < len(text) and text[end] not in {"\r", "\n"}:
            end += 1
        if end == len(text):
            result.append(PlainTextLine(line_number=line_number, text=text[cursor:end], ending=""))
            break
        ending = "\r\n" if text.startswith("\r\n", end) else text[end]
        result.append(PlainTextLine(line_number=line_number, text=text[cursor:end], ending=ending))
        cursor = end + len(ending)
        line_number += 1
    return tuple(result)


def apply_reviewed_plain_text_lines(
    text: str,
    replacements: Sequence[PlainTextReplacement],
    *,
    reviewed: bool,
) -> str:
    """按自然行号和完整原文写回译文，未选行及全部换行字节保持不变。"""

    if not reviewed:
        fail("分段纯文本写回", "没有明确的审核决定", "先确认 Generic 七项证据和逐行写回映射")
    by_line: dict[int, PlainTextReplacement] = {}
    for replacement in replacements:
        if replacement.line_number <= 0:
            fail("分段纯文本写回", "line_number 不是正整数", "使用 plain_text_lines 返回的自然行号")
        if replacement.line_number in by_line:
            fail(
                "分段纯文本写回",
                f"第 {replacement.line_number} 行出现多项替换",
                "每个自然行只保留一个审核决定",
            )
        if not replacement.translation or any(
            character in replacement.translation for character in ("\r", "\n", "\x00")
        ):
            fail(
                "分段纯文本写回",
                f"第 {replacement.line_number} 行译文为空或含换行/NUL",
                "单行译文必须非空；跨行来源使用经审核的专用转换",
            )
        by_line[replacement.line_number] = replacement
    lines = list(plain_text_lines(text))
    for line_number in by_line:
        if line_number > len(lines):
            fail(
                "分段纯文本写回",
                f"第 {line_number} 行超出当前来源的 {len(lines)} 行",
                "来源变化后重新提取并审核，不要套用旧决定",
            )
    rendered: list[str] = []
    for line in lines:
        replacement = by_line.get(line.line_number)
        if replacement is None:
            rendered.append(f"{line.text}{line.ending}")
            continue
        if replacement.source != line.text:
            fail(
                "分段纯文本写回",
                f"第 {line.line_number} 行当前原文与审核决定不一致",
                "来源变化后重新提取并审核，不要模糊匹配",
            )
        rendered.append(f"{replacement.translation}{line.ending}")
    return "".join(rendered)
