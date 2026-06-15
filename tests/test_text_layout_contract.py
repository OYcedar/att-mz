"""Python 与 Rust 共享的文本布局契约测试。"""

from __future__ import annotations

import json
from pathlib import Path
from typing import cast

from app.config.schemas import TextRulesSetting
from app.rmmz.text_layout import count_line_width_chars, split_overwide_lines
from app.rmmz.text_rules import JsonObject, JsonValue, TextRules, coerce_json_value

CONTRACT_PATH = Path(__file__).with_name("layout_contract_cases.json")


def _load_contract() -> JsonObject:
    """读取跨语言共享布局用例。"""
    value = coerce_json_value(cast(object, json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))))
    if not isinstance(value, dict):
        raise TypeError("布局契约用例必须是 JSON 对象")
    return value


def _contract_text_rules() -> TextRules:
    """构造共享契约测试用文本规则。"""
    return TextRules.from_setting(
        TextRulesSetting(
            line_width_count_pattern=r"\S",
            long_text_line_width_limit=999,
            preserve_wrapping_punctuation_pairs=[("「", "」"), ("『", "』"), ("（", "）")],
        )
    )


def _case_list(contract: JsonObject, key: str) -> list[JsonObject]:
    value = contract.get(key)
    if not isinstance(value, list):
        raise TypeError(f"{key} 必须是数组")
    cases: list[JsonObject] = []
    for item in value:
        if not isinstance(item, dict):
            raise TypeError(f"{key} 中的用例必须是对象")
        cases.append(item)
    return cases


def _string_field(case: JsonObject, key: str) -> str:
    value = case.get(key)
    if not isinstance(value, str):
        raise TypeError(f"{key} 必须是字符串")
    return value


def _int_field(case: JsonObject, key: str) -> int:
    value = case.get(key)
    if not isinstance(value, int):
        raise TypeError(f"{key} 必须是整数")
    return value


def _string_list_field(case: JsonObject, key: str) -> list[str]:
    value: JsonValue | None = case.get(key)
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise TypeError(f"{key} 必须是字符串数组")
    return [item for item in value if isinstance(item, str)]


def test_shared_layout_contract_counts_only_visible_width() -> None:
    """行宽统计只计算玩家可见文本，不计算控制符。"""
    rules = _contract_text_rules()
    for case in _case_list(_load_contract(), "line_width_cases"):
        assert count_line_width_chars(_string_field(case, "text"), rules) == _int_field(
            case,
            "expected_width",
        )


def test_shared_layout_contract_applies_wrapping_continuation_indent() -> None:
    """跨行包裹标点内部续行补全角空格，并保留控制符顺序。"""
    rules = _contract_text_rules()
    for case in _case_list(_load_contract(), "split_cases"):
        actual = split_overwide_lines(
            lines=_string_list_field(case, "lines"),
            location_path=_string_field(case, "name"),
            text_rules=rules,
        )
        assert actual == _string_list_field(case, "expected")
