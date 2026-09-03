"""RPG Maker 文本位置提示和与控制证明无关的结构工具。"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Literal

from att_skill_tools import JsonValue

ConsumerProfile = Literal["plain_text", "extended_text", "message_text"]


@dataclass(frozen=True, slots=True)
class ControlContract:
    """只描述静态调查得到的消费者提示；它不是强 Placeholder 契约。"""

    consumer: ConsumerProfile
    format_arity: int | None = None

    def json(self) -> dict[str, JsonValue]:
        value: dict[str, JsonValue] = {"consumer": self.consumer}
        if self.format_arity is not None:
            value["format_arity"] = self.format_arity
        return value


PLAIN_TEXT = ControlContract("plain_text")
EXTENDED_TEXT = ControlContract("extended_text")
MESSAGE_TEXT = ControlContract("message_text")

_DATABASE_EXTENDED_FIELDS = frozenset(
    {
        ("Actors.json", "profile"),
        ("Skills.json", "description"),
        ("Items.json", "description"),
        ("Weapons.json", "description"),
        ("Armors.json", "description"),
    }
)
_DATABASE_MESSAGE_UNION_FIELDS = frozenset(
    {
        ("Actors.json", "name"),
        ("Enemies.json", "name"),
        ("Skills.json", "name"),
        ("Items.json", "name"),
        ("Weapons.json", "name"),
        ("Armors.json", "name"),
    }
)
_SYSTEM_FORMAT_ARITY: dict[str, int] = {
    "expTotal": 1,
    "expNext": 1,
    "partyName": 1,
    "emerge": 1,
    "preemptive": 1,
    "surprise": 1,
    "escapeStart": 1,
    "victory": 1,
    "defeat": 1,
    "obtainGold": 1,
    "obtainItem": 1,
    "obtainSkill": 1,
    "actorNoDamage": 1,
    "actorNoHit": 1,
    "enemyNoDamage": 1,
    "enemyNoHit": 1,
    "evasion": 1,
    "magicEvasion": 1,
    "magicReflection": 1,
    "counterAttack": 1,
    "actionFailure": 1,
    "obtainExp": 2,
    "useItem": 2,
    "actorDamage": 2,
    "enemyDamage": 2,
    "substitute": 2,
    "buffAdd": 2,
    "debuffAdd": 2,
    "buffRemove": 2,
    "levelUp": 3,
    "actorRecovery": 3,
    "actorGain": 3,
    "actorLoss": 3,
    "actorDrain": 3,
    "enemyRecovery": 3,
    "enemyGain": 3,
    "enemyLoss": 3,
    "enemyDrain": 3,
}
_SYSTEM_MESSAGE_KEYS = frozenset(
    {
        "emerge",
        "partyName",
        "preemptive",
        "surprise",
        "victory",
        "defeat",
        "escapeStart",
        "escapeFailure",
        "obtainExp",
        "obtainGold",
        "obtainItem",
        "levelUp",
        "obtainSkill",
    }
)
_SYSTEM_EXTENDED_KEYS = frozenset(
    {
        "saveMessage",
        "loadMessage",
        "criticalToEnemy",
        "criticalToActor",
        *(
            key
            for key in _SYSTEM_FORMAT_ARITY
            if key not in _SYSTEM_MESSAGE_KEYS and key not in {"expTotal", "expNext", "partyName"}
        ),
    }
)
_FORMAT_ARGUMENT = re.compile(r"%[0-9]+")
_INDEXED_CONTROL = re.compile(
    r"(?:\\|\x1b)(?P<command>PX|PY|FS|V|N|P|C|I)\[(?P<argument>[0-9]+)\]",
    re.IGNORECASE | re.ASCII,
)
_MV_BARE_EXTENDED_CONTROL = re.compile(r"(?:\\|\x1b)(?:C|I)(?![A-Za-z])", re.IGNORECASE | re.ASCII)
_MZ_BARE_EXTENDED_CONTROL = re.compile(
    r"(?:\\|\x1b)(?:C|I|PX|PY|FS)(?![A-Za-z])",
    re.IGNORECASE | re.ASCII,
)
_RPG_LINE_BREAK = re.compile(r"\r\n|\r|\n")


def is_structural_blank(text: str) -> bool:
    """判断不会形成可见内容或已确认控制语义的纯空白。"""

    return all(character != "\f" and character.isspace() for character in text)


def split_rpg_text_lines(text: str) -> list[str]:
    """只按 RPG 文本容器实际使用的 CR/LF 分行，保留 FF。"""

    if not text:
        return []
    lines = _RPG_LINE_BREAK.split(text)
    if text.endswith(("\r", "\n")):
        lines.pop()
    return lines


def database_control_contract(engine: str, file_name: str, field_name: str) -> ControlContract:
    """返回标准字段的消费者提示，不把它升级为强契约。"""

    if file_name == "Skills.json" and field_name in {"message1", "message2"}:
        return ControlContract("extended_text", 1 if engine == "mv" else 2)
    if file_name == "States.json" and field_name in {"message1", "message2", "message3", "message4"}:
        consumer: ConsumerProfile = (
            "message_text" if field_name in {"message1", "message4"} else "extended_text"
        )
        return ControlContract(consumer, 1 if engine == "mz" else None)
    if (file_name, field_name) in _DATABASE_MESSAGE_UNION_FIELDS:
        return MESSAGE_TEXT
    if (file_name, field_name) in _DATABASE_EXTENDED_FIELDS:
        return EXTENDED_TEXT
    return PLAIN_TEXT


def system_control_contract(path: tuple[str | int, ...]) -> ControlContract:
    """返回 System 标准字段的消费者提示。"""

    if len(path) == 3 and path[:2] == ("terms", "basic") and isinstance(path[2], int):
        if path[2] in {0, 8}:
            return MESSAGE_TEXT
        if path[2] in {2, 4, 6}:
            return EXTENDED_TEXT
        return PLAIN_TEXT
    if len(path) == 3 and path[:2] == ("terms", "params") and isinstance(path[2], int):
        return EXTENDED_TEXT
    if len(path) != 3 or path[:2] != ("terms", "messages") or not isinstance(path[2], str):
        return PLAIN_TEXT
    key = path[2]
    arity = _SYSTEM_FORMAT_ARITY.get(key)
    if key in _SYSTEM_MESSAGE_KEYS:
        return ControlContract("message_text", arity)
    if key in _SYSTEM_EXTENDED_KEYS:
        return ControlContract("extended_text", arity)
    return ControlContract("plain_text", arity)


def event_control_contract(kind: str, role: str | None = None, engine: str | None = None) -> ControlContract:
    """返回标准事件位置的消费者提示。"""

    if kind == "event_dialogue" and role != "speaker":
        return MESSAGE_TEXT
    if kind == "event_dialogue" and role == "speaker":
        return EXTENDED_TEXT if engine == "mz" else PLAIN_TEXT
    if kind in {"event_choices", "event_scrolling_text"}:
        return EXTENDED_TEXT
    if kind == "event_command" and role == "name":
        return MESSAGE_TEXT
    if kind == "event_command" and role == "profile":
        return EXTENDED_TEXT
    return PLAIN_TEXT


def builtin_control_spans(
    engine: str,
    text: str,
    contract: ControlContract,
) -> list[tuple[int, int, str]]:
    """按 ATT 的 MV/MZ 默认事实找出当前标准消费者中的内建控制符。"""

    spans: list[tuple[int, int, str]] = []
    if contract.format_arity is not None:
        spans.extend(
            (match.start(), match.end(), "format_argument") for match in _FORMAT_ARGUMENT.finditer(text)
        )
    if contract.consumer == "plain_text":
        return sorted(spans)

    position = 0
    while position < len(text):
        character = text[position]
        if character == "\f":
            if contract.consumer == "message_text":
                spans.append((position, position + 1, "message_page_break"))
            position += 1
            continue
        if character not in {"\\", "\x1b"}:
            position += 1
            continue
        if position + 1 < len(text) and text[position + 1] in {"\\", "\x1b"}:
            spans.append((position, position + 2, "literal_introducer"))
            position += 2
            continue

        indexed = _INDEXED_CONTROL.match(text, position)
        if indexed is not None:
            command = indexed.group("command").upper()
            if command not in {"PX", "PY", "FS"} or engine == "mz":
                spans.append((position, indexed.end(), "indexed_control"))
                position = indexed.end()
                continue

        bare = (_MZ_BARE_EXTENDED_CONTROL if engine == "mz" else _MV_BARE_EXTENDED_CONTROL).match(
            text, position
        )
        if bare is not None:
            spans.append((position, bare.end(), "extended_control"))
            position = bare.end()
            continue

        if position + 1 < len(text):
            command = text[position + 1]
            if command in "Gg{}":
                spans.append((position, position + 2, "extended_control"))
                position += 2
                continue
            if contract.consumer == "message_text" and command in "$.|!><^":
                spans.append((position, position + 2, "message_control"))
                position += 2
                continue
        position += 1
    return sorted(spans)


def _outside_format_arity(digits: str, arity: int) -> bool:
    normalized = digits.lstrip("0") or "0"
    if normalized == "0":
        return True
    limit = str(arity)
    return len(normalized) > len(limit) or (len(normalized) == len(limit) and normalized > limit)


def unprotected_format_arguments(text: str, contract: ControlContract) -> list[tuple[int, int, str, str]]:
    """返回没有默认消费者依据或源参数越界的 `%N` Review。"""

    output: list[tuple[int, int, str, str]] = []
    for match in _FORMAT_ARGUMENT.finditer(text):
        if contract.format_arity is None:
            reason = "consumer_not_confirmed"
        elif _outside_format_arity(match.group(0)[1:], contract.format_arity):
            reason = "invalid_source_format_argument"
        else:
            continue
        output.append((match.start(), match.end(), match.group(0), reason))
    return output
