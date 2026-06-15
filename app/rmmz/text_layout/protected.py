"""RPG Maker 文本布局中的受保护片段识别。"""

from __future__ import annotations

from app.rmmz.text_rules import TextRules

from .models import ProtectedSpan


def collect_protected_spans(text: str, text_rules: TextRules) -> list[ProtectedSpan]:
    """收集占位符和 RPG Maker 控制符范围。"""
    spans = [
        ProtectedSpan(start_index=match.start(), end_index=match.end())
        for match in text_rules.placeholder_token_pattern.finditer(text)
    ]
    spans.extend(
        ProtectedSpan(start_index=span.start_index, end_index=span.end_index)
        for span in text_rules.iter_control_sequence_spans(text)
    )
    spans.extend(
        ProtectedSpan(start_index=candidate.start_index, end_index=candidate.end_index)
        for candidate in text_rules.iter_unprotected_control_sequence_candidates(text)
    )
    return _normalize_protected_spans(spans)


def strip_protected_spans(*, text: str, protected_spans: list[ProtectedSpan]) -> str:
    """返回剥离受保护片段后的可见文本。"""
    if not protected_spans:
        return text
    parts: list[str] = []
    last_end = 0
    for span in _normalize_protected_spans(protected_spans):
        if span.start_index > last_end:
            parts.append(text[last_end:span.start_index])
        last_end = span.end_index
    parts.append(text[last_end:])
    return "".join(parts)


def is_inside_protected_span(*, index: int, protected_spans: list[ProtectedSpan]) -> bool:
    """判断字符位置是否位于受保护片段内部。"""
    return any(span.start_index <= index < span.end_index for span in protected_spans)


def find_containing_span(*, index: int, protected_spans: list[ProtectedSpan]) -> ProtectedSpan | None:
    """返回包含指定字符下标的受保护片段。"""
    for span in protected_spans:
        if span.start_index <= index < span.end_index:
            return span
    return None


def move_split_position_outside_protected_span(
    *,
    position: int,
    protected_spans: list[ProtectedSpan],
) -> int:
    """把切分点移动到受保护片段之后，避免破坏控制符。"""
    for span in protected_spans:
        if span.start_index < position < span.end_index:
            return span.end_index
    return position


def _normalize_protected_spans(spans: list[ProtectedSpan]) -> list[ProtectedSpan]:
    """合并重叠或相邻的受保护片段。"""
    sorted_spans = sorted(
        (span for span in spans if span.start_index < span.end_index),
        key=lambda span: (span.start_index, span.end_index),
    )
    normalized: list[ProtectedSpan] = []
    for span in sorted_spans:
        if not normalized:
            normalized.append(span)
            continue
        previous = normalized[-1]
        if span.start_index <= previous.end_index:
            normalized[-1] = ProtectedSpan(
                start_index=previous.start_index,
                end_index=max(previous.end_index, span.end_index),
            )
            continue
        normalized.append(span)
    return normalized
