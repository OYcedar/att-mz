"""ATT Manual 自然 ID 与 Rules 来源投影。"""

from __future__ import annotations

import json
from collections import defaultdict
from collections.abc import Mapping, Sequence

from att_skill_tools import JsonValue

from .rpg import STANDARD_DATA_FILES, PluginInfo, canonical_map_number


def readable_component(value: str) -> str:
    if (
        value
        and (value[0] == "_" or value[0].isascii() and value[0].isalpha())
        and all(character == "_" or character.isascii() and character.isalnum() for character in value[1:])
    ):
        return value
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def _append_location_steps(
    output: str,
    steps: Sequence[str | int | None],
    *,
    source_kind: str,
) -> str:
    for position, step in enumerate(steps):
        if step is None:
            continue
        if isinstance(step, int):
            is_database_id = position == 0 and source_kind == "data"
            is_map_event_id = source_kind == "map" and position > 0 and steps[position - 1] == "events"
            number = step if is_database_id or is_map_event_id else step + 1
            output += f":{number}"
        else:
            component = readable_component(step)
            output += f":{component}" if component == step else f"[{component}]"
    return output


def _data_source_kind(source_file: str) -> str:
    if source_file in STANDARD_DATA_FILES:
        return "data"
    if canonical_map_number(source_file) is not None:
        return "map"
    return "data_file"


def manual_id(
    *,
    source_kind: str,
    source_file: str,
    group_steps: Sequence[str | int | None],
    kind: str,
    role: str | None,
    plugin: PluginInfo | None = None,
    parameter_name: str | None = None,
) -> str:
    if source_kind == "plugin":
        if plugin is None or parameter_name is None:
            raise AssertionError("插件 Manual ID 需要插件和参数名")
        output = (
            f"plugins.js:plugin{plugin.index + 1}:"
            f"{readable_component(plugin.name)}:{readable_component(parameter_name)}"
        )
        output = _append_location_steps(output, group_steps, source_kind="plugin")
    elif source_kind == "map":
        output = source_file
        if (
            len(group_steps) == 6
            and group_steps[0] == "events"
            and isinstance(group_steps[1], int)
            and group_steps[2] == "pages"
            and isinstance(group_steps[3], int)
            and group_steps[4] == "list"
            and isinstance(group_steps[5], int)
            and kind in {"event_dialogue", "event_choices", "event_scrolling_text", "event_command"}
        ):
            labels = {
                "event_dialogue": "dialogue",
                "event_choices": "choices",
                "event_scrolling_text": "scrolling",
                "event_command": "command",
            }
            output += f":event{group_steps[1]}:page{group_steps[3] + 1}:{labels[kind]}{group_steps[5] + 1}"
        else:
            output = _append_location_steps(output, group_steps, source_kind="map")
    else:
        output = _append_location_steps(
            source_file,
            group_steps,
            source_kind=source_kind,
        )
    if role is not None:
        output += f":{readable_component(role)}"
    return output


def _steps_with_decodes(
    path: Sequence[str | int], decode_positions: Sequence[int]
) -> tuple[str | int | None, ...]:
    positions: dict[int, int] = defaultdict(int)
    for position in decode_positions:
        positions[position] += 1
    result: list[str | int | None] = []
    for index, step in enumerate(path):
        result.extend([None] * positions[index])
        result.append(step)
    result.extend([None] * positions[len(path)])
    return tuple(result)


def rule_manual_id(
    fact_path: Sequence[str | int],
    decode_positions: Sequence[int],
    *,
    source_file: str,
    plugin: PluginInfo | None = None,
    command_group_steps: Sequence[str | int] | None = None,
    command_path_has_index: bool = False,
) -> str:
    steps = _steps_with_decodes(fact_path, decode_positions)
    if command_group_steps is not None and not command_path_has_index:
        group_steps: tuple[str | int | None, ...] = tuple(command_group_steps)
    else:
        non_decode = [index for index, step in enumerate(steps) if step is not None]
        if not non_decode:
            group_steps = tuple(command_group_steps or ())
        else:
            last = non_decode[-1]
            group_steps = steps if isinstance(steps[last], int) else steps[:last]
    relative = steps[len(group_steps) :]
    role_parts: list[str] = []
    for step in relative:
        if step is None:
            role_parts.append("<json>")
        elif isinstance(step, int):
            role_parts.append(f"[{step}]")
        else:
            role_parts.append(f"[{json.dumps(step, ensure_ascii=False, separators=(',', ':'))}]")
    if role_parts:
        role_parts.append(".")
    role_parts.append("text[0]")
    if plugin is not None:
        parameter = str(fact_path[0])
        plugin_steps = steps[1:]
        plugin_group = group_steps[1:] if group_steps else ()
        relative = plugin_steps[len(plugin_group) :]
        role_parts = []
        for step in relative:
            if step is None:
                role_parts.append("<json>")
            elif isinstance(step, int):
                role_parts.append(f"[{step}]")
            else:
                role_parts.append(f"[{json.dumps(step, ensure_ascii=False, separators=(',', ':'))}]")
        if role_parts:
            role_parts.append(".")
        role_parts.append("text[0]")
        return manual_id(
            source_kind="plugin",
            source_file="plugins.js",
            group_steps=plugin_group,
            kind="plugin_parameter",
            role="".join(role_parts),
            plugin=plugin,
            parameter_name=parameter,
        )
    return manual_id(
        source_kind=_data_source_kind(source_file),
        source_file=source_file,
        group_steps=group_steps,
        kind="event_command" if command_group_steps is not None else "database_entry",
        role="".join(role_parts),
    )


def rule_key(rule: Mapping[str, JsonValue]) -> str:
    return json.dumps(rule, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def source_name_for_rule(rule: Mapping[str, JsonValue]) -> str:
    if isinstance(rule.get("plugin"), str):
        return f"plugin:{rule['plugin']}"
    if isinstance(rule.get("file"), str):
        return f"data:{rule['file']}"
    return f"command:{rule.get('code')}:{rule.get('parameter')}"
