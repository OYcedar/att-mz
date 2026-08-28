"""标准 RPG Maker 数据和事件位置建立。"""

from __future__ import annotations

from collections import defaultdict
from collections.abc import Iterator, Mapping, Sequence

from att_skill_tools import JsonValue, fail

from .rpg import BUILTIN_DATABASE_FIELDS, actual_path, canonical_map_number
from .rpg_control_codes import (
    database_control_contract,
    event_control_contract,
    is_structural_blank,
    system_control_contract,
)
from .survey_identity import manual_id, readable_component
from .survey_model import EventFact, LocationFact


def event_lists(documents: Mapping[str, JsonValue], canonical_maps: Mapping[str, int]) -> Iterator[EventFact]:
    def commands(
        source_file: str,
        source_kind: str,
        list_steps: tuple[str | int, ...],
        value: JsonValue,
    ) -> Iterator[EventFact]:
        if not isinstance(value, list):
            fail(source_file, f"{actual_path(list_steps)} 不是事件命令数组", "修正损坏的 RPG Maker 数据")
        for command_index, command in enumerate(value):
            if not isinstance(command, dict):
                fail(source_file, "事件命令不是 object", "修正损坏的 RPG Maker 事件命令")
            code = command.get("code")
            parameters = command.get("parameters")
            if not isinstance(code, int) or isinstance(code, bool) or not isinstance(parameters, list):
                fail(source_file, "事件命令 code/parameters 类型无效", "修正损坏的 RPG Maker 事件命令")
            yield EventFact(
                source_file=source_file,
                source_kind=source_kind,
                list_steps=list_steps,
                command_index=command_index,
                code=code,
                parameters=tuple(parameters),
            )

    for name, _map_number in sorted(canonical_maps.items(), key=lambda item: item[1]):
        root = documents[name]
        if not isinstance(root, dict):
            fail(name, "Map 的 events 不是 array", "修正损坏的 Map JSON")
        events_value = root.get("events")
        if not isinstance(events_value, list):
            fail(name, "Map 的 events 不是 array", "修正损坏的 Map JSON")
        for event_index, event in enumerate(events_value):
            if event is None:
                continue
            if not isinstance(event, dict):
                fail(name, f"event{event_index} 的 pages 无效", "修正损坏的地图事件")
            pages_value = event.get("pages")
            if not isinstance(pages_value, list):
                fail(name, f"event{event_index} 的 pages 无效", "修正损坏的地图事件")
            for page_index, page in enumerate(pages_value):
                if not isinstance(page, dict):
                    fail(name, "事件页不是 object", "修正损坏的地图事件页")
                yield from commands(
                    name,
                    "map",
                    ("events", event_index, "pages", page_index, "list"),
                    page.get("list"),
                )
    common = documents.get("CommonEvents.json")
    if common is not None:
        if not isinstance(common, list):
            fail("CommonEvents.json", "根值不是 array", "修正损坏的标准数据")
        for event_index, event in enumerate(common):
            if event is None:
                continue
            if not isinstance(event, dict):
                fail("CommonEvents.json", "事件不是 object", "修正损坏的标准数据")
            yield from commands(
                "CommonEvents.json",
                "data",
                (event_index, "list"),
                event.get("list"),
            )
    troops = documents.get("Troops.json")
    if troops is not None:
        if not isinstance(troops, list):
            fail("Troops.json", "根值不是 array", "修正损坏的标准数据")
        for troop_index, troop in enumerate(troops):
            if troop is None:
                continue
            if not isinstance(troop, dict):
                fail("Troops.json", "敌群事件 pages 无效", "修正损坏的标准数据")
            pages_value = troop.get("pages")
            if not isinstance(pages_value, list):
                fail("Troops.json", "敌群事件 pages 无效", "修正损坏的标准数据")
            for page_index, page in enumerate(pages_value):
                if not isinstance(page, dict):
                    fail("Troops.json", "敌群事件页不是 object", "修正损坏的标准数据")
                yield from commands(
                    "Troops.json",
                    "data",
                    (troop_index, "pages", page_index, "list"),
                    page.get("list"),
                )


def builtin_database_locations(
    documents: Mapping[str, JsonValue],
    relative_files: Mapping[str, str],
    engine: str,
) -> list[LocationFact]:
    output: list[LocationFact] = []
    for file_name, fields in BUILTIN_DATABASE_FIELDS.items():
        root = documents.get(file_name)
        if root is None:
            continue
        if not isinstance(root, list):
            fail(file_name, "Builtin 数据文件根值不是 array", "修正损坏的标准 RPG Maker 数据")
        for index, entry in enumerate(root):
            if entry is None:
                continue
            if not isinstance(entry, dict):
                fail(file_name, f"第 {index} 项不是 object", "修正损坏的标准 RPG Maker 数据")
            for field_name in fields:
                text = entry.get(field_name)
                if not isinstance(text, str):
                    fail(file_name, f"第 {index} 项 {field_name} 不是 string", "恢复标准字段")
                if is_structural_blank(text):
                    continue
                output.append(
                    LocationFact(
                        source=f"data/{file_name}:builtin",
                        location=f"{file_name}:{index}:{field_name}",
                        source_text=text,
                        classification="builtin",
                        physical_file=relative_files[file_name],
                        json_path=(index, field_name),
                        expected_manual_id=f"{file_name}:{index}:{readable_component(field_name)}",
                        control_contract=database_control_contract(engine, file_name, field_name).json(),
                        roles={"display"},
                    )
                )
    return output


def builtin_system_locations(
    documents: Mapping[str, JsonValue], relative_files: Mapping[str, str]
) -> list[LocationFact]:
    root = documents.get("System.json")
    if not isinstance(root, dict):
        fail("System.json", "根值不是 object", "修正损坏的标准 RPG Maker 数据")
    output: list[LocationFact] = []
    relative = relative_files["System.json"]

    def add(path: tuple[str | int, ...], role: str, text: JsonValue) -> None:
        if not isinstance(text, str):
            fail("System.json", f"{actual_path(path)} 不是 string", "恢复标准 System 字段")
        if is_structural_blank(text):
            return
        group_steps = path[:-1] if len(path) > 1 else ()
        output.append(
            LocationFact(
                source="data/System.json:builtin",
                location=f"System.json:{actual_path(path)}",
                source_text=text,
                classification="builtin",
                physical_file=relative,
                json_path=path,
                expected_manual_id=manual_id(
                    source_kind="data",
                    source_file="System.json",
                    group_steps=group_steps,
                    kind="system",
                    role=role,
                ),
                control_contract=system_control_contract(path).json(),
                roles={"display"},
            )
        )

    for field_name in ("gameTitle", "currencyUnit"):
        add((field_name,), field_name, root.get(field_name))
    terms = root.get("terms")
    if not isinstance(terms, dict):
        fail("System.json", "terms 不是 object", "恢复标准 System terms")
    for field_name in ("basic", "commands", "params"):
        values = terms.get(field_name)
        if not isinstance(values, list):
            fail("System.json", f"terms.{field_name} 不是 array", "恢复标准 System terms")
        for index, text in enumerate(values):
            if text is not None:
                add(("terms", field_name, index), f"terms.{field_name}[{index}]", text)
    messages = terms.get("messages")
    if not isinstance(messages, dict):
        fail("System.json", "terms.messages 不是 object", "恢复标准 System terms.messages")
    for key, text in messages.items():
        add(("terms", "messages", key), f"terms.messages.{key}", text)
    for field_name in ("elements", "skillTypes", "weaponTypes", "armorTypes", "equipTypes"):
        values = root.get(field_name)
        if not isinstance(values, list):
            fail("System.json", f"{field_name} 不是 array", "恢复标准 System 字段")
        for index, text in enumerate(values):
            if text is not None:
                add((field_name, index), f"{field_name}[{index}]", text)
    return output


def builtin_event_locations(
    events: Sequence[EventFact],
    documents: Mapping[str, JsonValue],
    relative_files: Mapping[str, str],
    engine: str,
) -> list[LocationFact]:
    output: list[LocationFact] = []
    by_list: dict[tuple[str, tuple[str | int, ...]], list[EventFact]] = defaultdict(list)
    for event in events:
        by_list[(event.source_file, event.list_steps)].append(event)
    for commands in by_list.values():
        commands.sort(key=lambda item: item.command_index)
        positions = {command.command_index: command for command in commands}
        for command in commands:
            group_steps = command.command_steps
            relative_file = relative_files[command.source_file]
            if command.code == 101:
                lines: list[str] = []
                physical: list[tuple[str | int, ...]] = []
                next_index = command.command_index + 1
                while (next_command := positions.get(next_index)) is not None and next_command.code == 401:
                    text = next_command.parameters[0] if next_command.parameters else None
                    if not isinstance(text, str):
                        fail(command.source_file, "401 正文不是 string", "修正损坏的事件对话")
                    lines.append(text)
                    physical.append((*next_command.command_steps, "parameters", 0))
                    next_index += 1
                if not lines:
                    continue
                if engine == "mz" and len(command.parameters) > 4:
                    speaker = command.parameters[4]
                    if not isinstance(speaker, str):
                        fail(command.source_file, "MZ Speaker 不是 string", "修正损坏的 101 参数")
                    if not is_structural_blank(speaker):
                        output.append(
                            LocationFact(
                                source=f"data/{command.source_file}:builtin-events",
                                location=f"{command.source_file}:{actual_path(group_steps)}:speaker",
                                source_text=speaker,
                                classification="builtin",
                                physical_file=relative_file,
                                json_path=(*group_steps, "parameters", 4),
                                expected_manual_id=manual_id(
                                    source_kind=command.source_kind,
                                    source_file=command.source_file,
                                    group_steps=group_steps,
                                    kind="event_dialogue",
                                    role="speaker",
                                ),
                                control_contract=event_control_contract(
                                    "event_dialogue", "speaker", engine
                                ).json(),
                                roles={"display"},
                            )
                        )
                if any(not is_structural_blank(line) for line in lines):
                    output.append(
                        LocationFact(
                            source=f"data/{command.source_file}:builtin-events",
                            location=f"{command.source_file}:{actual_path(group_steps)}:body",
                            source_text="\n".join(lines),
                            classification="builtin",
                            physical_file=relative_file,
                            json_path=physical[0],
                            expected_manual_id=manual_id(
                                source_kind=command.source_kind,
                                source_file=command.source_file,
                                group_steps=group_steps,
                                kind="event_dialogue",
                                role=None,
                            ),
                            control_contract=event_control_contract("event_dialogue").json(),
                            roles={"display"},
                            dialogue_first_line=lines[0] if engine == "mv" else None,
                        )
                    )
            elif command.code == 102:
                choices = command.parameters[0] if command.parameters else None
                if not isinstance(choices, list) or not all(isinstance(item, str) for item in choices):
                    fail(command.source_file, "102 choices 不是 string array", "修正损坏的事件选项")
                choice_texts = [str(item) for item in choices]
                if any(not is_structural_blank(item) for item in choice_texts):
                    output.append(
                        LocationFact(
                            source=f"data/{command.source_file}:builtin-events",
                            location=f"{command.source_file}:{actual_path(group_steps)}:choices",
                            source_text="\n".join(choice_texts),
                            classification="builtin",
                            physical_file=relative_file,
                            json_path=(*group_steps, "parameters", 0),
                            expected_manual_id=manual_id(
                                source_kind=command.source_kind,
                                source_file=command.source_file,
                                group_steps=group_steps,
                                kind="event_choices",
                                role=None,
                            ),
                            control_contract=event_control_contract("event_choices").json(),
                            roles={"display"},
                        )
                    )
            elif command.code == 105:
                lines = []
                next_index = command.command_index + 1
                while (next_command := positions.get(next_index)) is not None and next_command.code == 405:
                    text = next_command.parameters[0] if next_command.parameters else None
                    if not isinstance(text, str):
                        fail(command.source_file, "405 正文不是 string", "修正损坏的滚动文本")
                    lines.append(text)
                    next_index += 1
                if any(not is_structural_blank(line) for line in lines):
                    output.append(
                        LocationFact(
                            source=f"data/{command.source_file}:builtin-events",
                            location=f"{command.source_file}:{actual_path(group_steps)}:scrolling",
                            source_text="\n".join(lines),
                            classification="builtin",
                            physical_file=relative_file,
                            json_path=(*group_steps, "parameters", 0),
                            expected_manual_id=manual_id(
                                source_kind=command.source_kind,
                                source_file=command.source_file,
                                group_steps=group_steps,
                                kind="event_scrolling_text",
                                role=None,
                            ),
                            control_contract=event_control_contract("event_scrolling_text").json(),
                            roles={"display"},
                        )
                    )
            elif command.code in {320, 324, 325}:
                field_name = {320: "name", 324: "nickname", 325: "profile"}[command.code]
                text = command.parameters[1] if len(command.parameters) > 1 else None
                if not isinstance(text, str):
                    fail(command.source_file, f"{command.code} 文本参数不是 string", "修正损坏的事件命令")
                if not is_structural_blank(text):
                    output.append(
                        LocationFact(
                            source=f"data/{command.source_file}:builtin-events",
                            location=f"{command.source_file}:{actual_path(group_steps)}:{field_name}",
                            source_text=text,
                            classification="builtin",
                            physical_file=relative_file,
                            json_path=(*group_steps, "parameters", 1),
                            expected_manual_id=manual_id(
                                source_kind=command.source_kind,
                                source_file=command.source_file,
                                group_steps=group_steps,
                                kind="event_command",
                                role=field_name,
                            ),
                            control_contract=event_control_contract("event_command", field_name).json(),
                            roles={"display"},
                        )
                    )
    for name in sorted(documents):
        map_number = canonical_map_number(name)
        if map_number is None:
            continue
        root = documents[name]
        if not isinstance(root, dict):
            continue
        display_name = root.get("displayName")
        if not isinstance(display_name, str):
            fail(name, "displayName 不是 string", "修正损坏的 Map JSON")
        if not is_structural_blank(display_name):
            output.append(
                LocationFact(
                    source=f"data/{name}:builtin",
                    location=f"{name}:displayName",
                    source_text=display_name,
                    classification="builtin",
                    physical_file=relative_files[name],
                    json_path=("displayName",),
                    expected_manual_id=f"{name}:displayName",
                    control_contract=event_control_contract("map").json(),
                    roles={"display"},
                )
            )
    return output


def is_builtin_command_leaf(event: EventFact, parameter: int, path: Sequence[str | int], engine: str) -> bool:
    if event.code in {401, 405} and parameter == 0:
        return True
    if event.code == 102 and parameter == 0:
        return True
    if event.code == 402 and parameter in {0, 1}:
        return True
    if event.code in {320, 324, 325} and parameter == 1:
        return True
    return engine == "mz" and event.code == 101 and parameter == 4 and not path
