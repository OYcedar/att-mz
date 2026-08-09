"""活动 RPG Maker 插件脚本的保守、只读 JavaScript 词法扫描。"""

from __future__ import annotations

import posixpath
import re
from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class JavaScriptLiteral:
    value: str
    line: int
    kind: str
    dynamic_template: bool


@dataclass(frozen=True, slots=True)
class JavaScriptScan:
    literals: tuple[JavaScriptLiteral, ...]
    code: str
    warnings: tuple[dict[str, int | str], ...]


_FUNCTION_MARKER = re.compile(r"\bfunction\b|=>")
_SCRIPT_LOADER = re.compile(r"\b(?:require|import|loadScript|loadPlugin|PluginManager\.[A-Za-z0-9_]+)\b")
_REGEX_PREFIX_KEYWORDS = {
    "await",
    "case",
    "delete",
    "do",
    "else",
    "in",
    "instanceof",
    "new",
    "of",
    "return",
    "throw",
    "typeof",
    "void",
    "yield",
}


def _hex_value(text: str) -> str | None:
    try:
        value = int(text, 16)
    except ValueError:
        return None
    try:
        return chr(value)
    except ValueError:
        return None


def _escaped(text: str, index: int) -> tuple[str, int]:
    """解码足以比较静态路径的 JavaScript 字符串 escape。"""

    if index >= len(text):
        return "", index
    character = text[index]
    simple = {
        "b": "\b",
        "f": "\f",
        "n": "\n",
        "r": "\r",
        "t": "\t",
        "v": "\v",
        "0": "\0",
    }
    if character in simple:
        return simple[character], index + 1
    if character == "\r":
        return "", index + 2 if index + 1 < len(text) and text[index + 1] == "\n" else index + 1
    if character == "\n":
        return "", index + 1
    if character == "x" and index + 2 < len(text):
        decoded = _hex_value(text[index + 1 : index + 3])
        if decoded is not None:
            return decoded, index + 3
    if character == "u":
        if index + 1 < len(text) and text[index + 1] == "{":
            end = text.find("}", index + 2)
            if end >= 0:
                decoded = _hex_value(text[index + 2 : end])
                if decoded is not None:
                    return decoded, end + 1
        elif index + 4 < len(text):
            decoded = _hex_value(text[index + 1 : index + 5])
            if decoded is not None:
                return decoded, index + 5
    return character, index + 1


def _quoted(text: str, start: int, quote: str) -> tuple[str, int, bool]:
    index = start + 1
    value: list[str] = []
    while index < len(text):
        character = text[index]
        if character == quote:
            return "".join(value), index + 1, True
        if character == "\\":
            decoded, index = _escaped(text, index + 1)
            value.append(decoded)
            continue
        if character in "\r\n":
            return "".join(value), index, False
        value.append(character)
        index += 1
    return "".join(value), index, False


def _skip_comment(text: str, start: int) -> tuple[int, bool]:
    if text.startswith("//", start):
        end = text.find("\n", start + 2)
        return (len(text) if end < 0 else end), True
    if text.startswith("/*", start):
        end = text.find("*/", start + 2)
        return (len(text) if end < 0 else end + 2), end >= 0
    return start, True


def _regex_literal(text: str, start: int) -> tuple[int, bool]:
    """跳过一个词法上已确定的 regex literal。"""

    index = start + 1
    in_character_class = False
    while index < len(text):
        character = text[index]
        if character == "\\":
            index += 2
            continue
        if character in "\r\n":
            return index, False
        if character == "[":
            in_character_class = True
        elif character == "]" and in_character_class:
            in_character_class = False
        elif character == "/" and not in_character_class:
            index += 1
            while index < len(text) and (text[index].isalpha() or text[index].isdigit()):
                index += 1
            return index, True
        index += 1
    return index, False


def _identifier_end(text: str, start: int) -> int:
    index = start + 1
    while index < len(text) and (text[index].isalnum() or text[index] in "_$"):
        index += 1
    return index


def static_code_targets(value: str, script_relative: str) -> tuple[str, ...]:
    """把静态 JS 路径字面量解析为范围内候选自然路径，不访问文件系统。"""

    normalized = value.replace("\\", "/").split("?", 1)[0].split("#", 1)[0]
    if (
        not normalized
        or normalized.startswith("/")
        or re.match(r"\A[A-Za-z]:", normalized) is not None
        or posixpath.splitext(normalized)[1].lower() not in {".js", ".mjs"}
    ):
        return ()
    parent = posixpath.dirname(script_relative)
    candidates = {
        posixpath.normpath(posixpath.join(parent, normalized)),
        posixpath.normpath(normalized),
    }
    return tuple(
        sorted(
            candidate
            for candidate in candidates
            if candidate not in {".", ".."} and not candidate.startswith("../")
        )
    )


def loader_call_on_line(code: str, line_number: int) -> bool:
    lines = code.splitlines()
    return 0 < line_number <= len(lines) and _SCRIPT_LOADER.search(lines[line_number - 1]) is not None


def _skip_template_expression(text: str, start: int) -> tuple[int, bool]:
    """从 `${` 后跳到配对 `}`，只用于避免把表达式误当静态文本。"""

    depth = 1
    index = start
    while index < len(text):
        if text.startswith("//", index) or text.startswith("/*", index):
            index, closed = _skip_comment(text, index)
            if not closed:
                return index, False
            continue
        character = text[index]
        if character in "'\"":
            _, index, closed = _quoted(text, index, character)
            if not closed:
                return index, False
            continue
        if character == "`":
            _, index, closed, _, _ = _template(text, index)
            if not closed:
                return index, False
            continue
        if character == "{":
            depth += 1
        elif character == "}":
            depth -= 1
            if depth == 0:
                return index + 1, True
        index += 1
    return index, False


def _template(text: str, start: int) -> tuple[list[tuple[str, int]], int, bool, bool, list[tuple[int, int]]]:
    index = start + 1
    line = text.count("\n", 0, start) + 1
    fragment_line = line
    fragment: list[str] = []
    fragments: list[tuple[str, int]] = []
    expressions: list[tuple[int, int]] = []
    dynamic = False
    while index < len(text):
        character = text[index]
        if character == "`":
            fragments.append(("".join(fragment), fragment_line))
            return fragments, index + 1, True, dynamic, expressions
        if character == "\\":
            decoded, next_index = _escaped(text, index + 1)
            fragment.append(decoded)
            line += text.count("\n", index, next_index)
            index = next_index
            continue
        if text.startswith("${", index):
            fragments.append(("".join(fragment), fragment_line))
            fragment = []
            dynamic = True
            expression_start = index + 2
            index, closed = _skip_template_expression(text, expression_start)
            if not closed:
                return fragments, index, False, dynamic, expressions
            expressions.append((expression_start, index - 1))
            line = text.count("\n", 0, index) + 1
            fragment_line = line
            continue
        fragment.append(character)
        if character == "\n":
            line += 1
        index += 1
    fragments.append(("".join(fragment), fragment_line))
    return fragments, index, False, dynamic, expressions


def scan_javascript(text: str) -> JavaScriptScan:
    """扫描字符串字面量，返回移除注释和字符串后的等行数代码。"""

    masked = list(text)
    literals: list[JavaScriptLiteral] = []
    warnings: list[dict[str, int | str]] = []
    index = 0
    line = 1
    can_start_regex = True
    ambiguous_slash_context = False
    while index < len(text):
        start = index
        character = text[index]
        if text.startswith("//", index) or text.startswith("/*", index):
            end, closed = _skip_comment(text, index)
            if not closed:
                warnings.append({"line": line, "kind": "unterminated_block_comment"})
            for position in range(start, end):
                if masked[position] not in "\r\n":
                    masked[position] = " "
            line += text.count("\n", start, end)
            index = end
            continue
        if character in "'\"":
            value, end, closed = _quoted(text, index, character)
            if closed:
                literals.append(
                    JavaScriptLiteral(value=value, line=line, kind="string", dynamic_template=False)
                )
            else:
                warnings.append({"line": line, "kind": "unterminated_string"})
            for position in range(start, end):
                if masked[position] not in "\r\n":
                    masked[position] = " "
            line += text.count("\n", start, end)
            index = end
            can_start_regex = False
            ambiguous_slash_context = False
            continue
        if character == "`":
            fragments, end, closed, dynamic, expressions = _template(text, index)
            for value, fragment_line in fragments:
                if value:
                    literals.append(
                        JavaScriptLiteral(
                            value=value,
                            line=fragment_line,
                            kind="template_static",
                            dynamic_template=dynamic,
                        )
                    )
            if dynamic:
                warnings.append({"line": line, "kind": "dynamic_template_requires_review"})
            if not closed:
                warnings.append({"line": line, "kind": "unterminated_template"})
            for position in range(start, end):
                if masked[position] not in "\r\n":
                    masked[position] = " "
            for expression_start, expression_end in expressions:
                nested = scan_javascript(text[expression_start:expression_end])
                line_offset = text.count("\n", 0, expression_start)
                literals.extend(
                    JavaScriptLiteral(
                        value=literal.value,
                        line=literal.line + line_offset,
                        kind=literal.kind,
                        dynamic_template=literal.dynamic_template,
                    )
                    for literal in nested.literals
                )
                warnings.extend(
                    {
                        "line": int(warning["line"]) + line_offset,
                        "kind": str(warning["kind"]),
                    }
                    for warning in nested.warnings
                )
                for offset, nested_character in enumerate(nested.code):
                    masked[expression_start + offset] = nested_character
            line += text.count("\n", start, end)
            index = end
            can_start_regex = False
            ambiguous_slash_context = False
            continue
        if character == "/":
            if can_start_regex:
                end, closed = _regex_literal(text, index)
                if not closed:
                    warnings.append({"line": line, "kind": "unterminated_regex_literal"})
                for position in range(start, end):
                    if masked[position] not in "\r\n":
                        masked[position] = " "
                line += text.count("\n", start, end)
                index = end
                can_start_regex = False
                ambiguous_slash_context = False
                continue
            if ambiguous_slash_context:
                warnings.append({"line": line, "kind": "ambiguous_slash_treated_as_division"})
            index += 2 if index + 1 < len(text) and text[index + 1] == "=" else 1
            can_start_regex = True
            ambiguous_slash_context = False
            continue
        if character.isalpha() or character in "_$":
            end = _identifier_end(text, index)
            can_start_regex = text[index:end] in _REGEX_PREFIX_KEYWORDS
            ambiguous_slash_context = False
            index = end
            continue
        if character.isdigit():
            index += 1
            while index < len(text) and (text[index].isalnum() or text[index] in "._"):
                index += 1
            can_start_regex = False
            ambiguous_slash_context = False
            continue
        if character in ")]}":
            can_start_regex = False
            ambiguous_slash_context = True
        elif character in "([{,;:=!?&|+-*%^~<>":
            can_start_regex = True
            ambiguous_slash_context = False
        elif character == ".":
            can_start_regex = False
            ambiguous_slash_context = False
        if character == "\n":
            line += 1
        index += 1
    return JavaScriptScan(literals=tuple(literals), code="".join(masked), warnings=tuple(warnings))


def function_scope_hints(code: str) -> dict[int, int | None]:
    """给每行一个保守的词法函数范围提示；它不证明真实 JavaScript 数据流。"""

    scopes: dict[int, int | None] = {}
    stack: list[tuple[int, int]] = []
    depth = 0
    pending_function: int | None = None
    for line_number, line in enumerate(code.splitlines(), start=1):
        scopes[line_number] = stack[-1][1] if stack else None
        if _FUNCTION_MARKER.search(line):
            pending_function = line_number
        for character in line:
            if character == "{":
                depth += 1
                if pending_function is not None:
                    stack.append((depth, pending_function))
                    pending_function = None
                    scopes[line_number] = stack[-1][1]
            elif character == "}":
                if stack and stack[-1][0] == depth:
                    stack.pop()
                depth = max(0, depth - 1)
    return scopes
