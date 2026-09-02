"""只产生审核建议、不改变覆盖或所有权的启发式。"""

from __future__ import annotations

import re
from collections.abc import Mapping

from att_skill_tools import JsonValue

from .rpg import looks_like_player_text

_ANGLE_WRAPPER = re.compile(r"\A(<[A-Za-z][^<>\r\n]{0,60}>)([\s\S]+)(</[A-Za-z][^<>\r\n]{0,60}>)\Z")
_ANGLE_LABEL = re.compile(r"\A(<[A-Za-z][^<>:\r\n]{0,40}:)([\s\S]+)(>)\Z")
_PUNCTUATION_WRAPPER = re.compile(
    r"\A([^\w\u3040-\u30ff\u3400-\u9fff]*)([\s\S]*?[\w\u3040-\u30ff\u3400-\u9fff])([^\w\u3040-\u30ff\u3400-\u9fff]*)\Z"
)


def lexical_suggestion(value: str) -> list[dict[str, JsonValue]]:
    """词形判断只减少 Agent 阅读成本，不改变候选覆盖或所有权。"""

    if looks_like_player_text(value):
        return []
    return [
        {
            "kind": "lexical_non_source_suggestion",
            "suggestion": "可能是内部值；仍需审核后才能排除",
            "analysis_status": "heuristic",
        }
    ]


def capture_pattern(value: str) -> tuple[str | None, dict[str, JsonValue] | None]:
    """为字节固定的外壳生成一条可逆 Rules 捕获方案。"""

    wrapper = _ANGLE_WRAPPER.fullmatch(value)
    if wrapper is not None:
        opening, body, closing = wrapper.groups()
        opening_name = opening[1:].split(None, 1)[0].removesuffix(">")
        closing_name = closing[2:-1]
        if body.strip() and opening_name == closing_name:
            pattern = rf"\A{re.escape(opening)}(?<text>(?s:.+?)){re.escape(closing)}\z"
            return pattern, {
                "kind": "generated_rule_pattern",
                "basis": "同一字符串具有名称一致的完整开始与结束标签；外壳逐字冻结",
                "analysis_status": "confirmed",
            }
    label = _ANGLE_LABEL.fullmatch(value)
    if label is not None and label.group(2).strip():
        prefix, _body, suffix = label.groups()
        pattern = rf"\A{re.escape(prefix)}(?<text>(?s:.+?)){re.escape(suffix)}\z"
        return pattern, {
            "kind": "generated_rule_pattern",
            "basis": "同一字符串具有完整尖括号键前缀和闭合符；外壳逐字冻结",
            "analysis_status": "confirmed",
        }
    punctuation = _PUNCTUATION_WRAPPER.fullmatch(value)
    if punctuation is not None:
        prefix, body, suffix = punctuation.groups()
        if (
            body.strip()
            and (prefix or suffix)
            and not prefix.endswith(("\\", "\x1b"))
            and not (prefix.endswith("%") and body[0].isdigit())
            and "]" not in body
            and ">" not in body
            and "]" not in suffix
            and ">" not in suffix
        ):
            pattern = rf"\A{re.escape(prefix)}(?<text>(?s:.+?)){re.escape(suffix)}\z"
            return pattern, {
                "kind": "generated_rule_pattern",
                "basis": "字符串首尾只有固定标点或空白；正文边界可逆",
                "analysis_status": "confirmed",
            }
    return None, None


def rule_proposal(
    base: Mapping[str, JsonValue], value: str
) -> tuple[dict[str, JsonValue], list[dict[str, JsonValue]]]:
    rule = dict(base)
    pattern, evidence = capture_pattern(value)
    if pattern is not None:
        rule["pattern"] = pattern
    return rule, [] if evidence is None else [evidence]
