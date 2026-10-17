"""按真实语境顺序产生非阻断的串句、相邻错位审校线索。"""

from __future__ import annotations

import math
import re
from collections import Counter
from collections.abc import Mapping, Sequence
from typing import cast

from att_skill_tools import JsonValue

from .translation_export import natural_segments

_HAN = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff]")
_QUOTED_PASSAGE = re.compile(r'(?m)^\s*[「『“"]')


def adjacent_review(
    rows: Sequence[Mapping[str, JsonValue]],
    scopes: Mapping[str, str],
    terminology: Sequence[tuple[str, str]],
    *,
    source_language: str,
    target_language: str,
) -> list[dict[str, JsonValue]]:
    """比较同一 Scope 中前后两个 Unit；只报告候选，不移动或重写译文。"""
    groups: dict[str, list[Mapping[str, JsonValue]]] = {}
    for row in rows:
        identifier = cast(str, row["manual_id"])
        groups.setdefault(scopes.get(identifier, cast(str, row["group"])), []).append(row)
    terms = sorted(terminology, key=lambda pair: len(pair[0]), reverse=True)

    def signature(text: str) -> set[str]:
        for source, target in terms:
            text = text.replace(source, target)
        return set(_HAN.findall(text))

    japanese_chinese = source_language.split("-")[0] == "ja" and target_language.split("-")[0] == "zh"
    frequencies: Counter[str] = Counter()
    source_sets = (
        {cast(str, row["manual_id"]): signature("\n".join(natural_segments(row, "source"))) for row in rows}
        if japanese_chinese
        else {}
    )
    for value in source_sets.values():
        frequencies.update(value)
    weights = {char: math.log((len(rows) + 1) / (count + 1)) + 1 for char, count in frequencies.items()}

    def score(source: set[str], target: set[str]) -> float:
        return sum(weights.get(char, 1) for char in source & target) / max(
            1.0, sum(weights.get(char, 1) for char in source)
        )

    findings: list[dict[str, JsonValue]] = []
    for members in groups.values():
        sources = ["\n".join(natural_segments(row, "source")) for row in members]
        targets = ["\n".join(natural_segments(row, "translation")) for row in members]
        normalized = ["".join(char for char in text if char.isalnum()) for text in targets]
        for index, row in enumerate(members):
            if row["state"] != "current":
                continue
            identifier = cast(str, row["manual_id"])
            start, end = max(0, index - 2), min(len(members), index + 3)
            window = list(range(start, end))
            kinds: list[tuple[str, dict[str, JsonValue]]] = []
            overlap = [
                cast(str, members[neighbor]["manual_id"])
                for neighbor in window
                if neighbor != index
                and members[neighbor]["state"] == "current"
                and sources[index] != sources[neighbor]
                and len(normalized[neighbor]) >= 12
                and normalized[neighbor] in normalized[index]
            ]
            if overlap:
                kinds.append(("adjacent_content_overlap", {"overlaps": overlap}))
            source_passages = len(_QUOTED_PASSAGE.findall(sources[index]))
            target_passages = len(_QUOTED_PASSAGE.findall(targets[index]))
            if source_passages and target_passages > source_passages:
                kinds.append(
                    (
                        "additional_quoted_passage",
                        {
                            "source_passages": source_passages,
                            "translation_passages": target_passages,
                        },
                    )
                )
            if (
                japanese_chinese
                and len(targets[index]) >= 80
                and len(targets[index]) > 4 * max(1, len(sources[index]))
            ):
                kinds.append(
                    (
                        "unexpected_expansion",
                        {
                            "source_characters": len(sources[index]),
                            "translation_characters": len(targets[index]),
                        },
                    )
                )
            if japanese_chinese and len(source_sets[identifier]) >= 4:
                target_set = signature(targets[index])
                own = score(source_sets[identifier], target_set)
                best, neighbor = max(
                    (score(source_sets[cast(str, members[neighbor]["manual_id"])], target_set), neighbor)
                    for neighbor in window
                    if len(source_sets[cast(str, members[neighbor]["manual_id"])]) >= 4
                )
                if neighbor != index and best >= 0.7 and best - own >= 0.4:
                    kinds.append(
                        (
                            "adjacent_alignment_review",
                            {
                                "neighbor_id": members[neighbor]["manual_id"],
                                "current_score": round(own, 3),
                                "neighbor_score": round(best, 3),
                            },
                        )
                    )
            for kind, detail in kinds:
                findings.append(
                    {
                        "kind": kind,
                        "analysis_status": "heuristic_review",
                        "manual_id": identifier,
                        "review_ids": [members[item]["manual_id"] for item in window],
                        "context": [
                            {
                                "manual_id": members[item]["manual_id"],
                                "source": sources[item],
                                "translation": targets[item],
                            }
                            for item in window
                        ],
                        **detail,
                    }
                )
    return findings
