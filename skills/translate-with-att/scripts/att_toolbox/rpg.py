"""RPG Maker 内容根、插件与事件的只读解析。"""

from __future__ import annotations

import json
import re
import sys
from collections.abc import Iterator, Mapping
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ensure_inside,
    fail,
    json_type,
    parse_json_prefix,
    parse_json_text,
    read_json,
    require_directory,
    require_file_within,
    safe_walk_files,
)

STANDARD_DATA_FILES = (
    "Actors.json",
    "Animations.json",
    "Armors.json",
    "Classes.json",
    "CommonEvents.json",
    "Enemies.json",
    "Items.json",
    "MapInfos.json",
    "Skills.json",
    "States.json",
    "System.json",
    "Tilesets.json",
    "Troops.json",
    "Weapons.json",
)
BUILTIN_DATABASE_FIELDS: dict[str, tuple[str, ...]] = {
    "Actors.json": ("name", "nickname", "profile"),
    "Classes.json": ("name",),
    "Skills.json": ("name", "description", "message1", "message2"),
    "Items.json": ("name", "description"),
    "Weapons.json": ("name", "description"),
    "Armors.json": ("name", "description"),
    "Enemies.json": ("name",),
    "States.json": ("name", "message1", "message2", "message3", "message4"),
}
BUILTIN_EVENT_CODES = frozenset({101, 102, 105, 320, 324, 325, 401, 402, 404, 405})
_SYSTEM_ARRAYS = {"elements", "skillTypes", "weaponTypes", "armorTypes", "equipTypes"}
_MAP_NAME = re.compile(r"Map([0-9]+)\.json\Z")
_BARE_KEY = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")
_PLUGINS_ASSIGNMENT = re.compile(r"(?:\b(?:var|let|const)\s+)?\$plugins\s*=")


@dataclass(frozen=True, slots=True)
class GameInfo:
    supplied_root: Path
    content_root: Path
    engine: str


@dataclass(frozen=True, slots=True)
class PluginInfo:
    name: str
    status: bool
    description: str
    parameters: dict[str, JsonValue]
    index: int


@dataclass(frozen=True, slots=True)
class EventCommand:
    source_file: str
    location: str
    index: int
    code: int
    parameters: list[JsonValue]


@dataclass(frozen=True, slots=True)
class StringLeaf:
    path: tuple[str | int, ...]
    value: str
    decoded_layers: int


def looks_like_player_text(value: str) -> bool:
    """只筛除空白、纯数字和纯标点；可见性仍由 Agent 判断。"""

    stripped = value.strip()
    return bool(stripped) and any(character.isalpha() for character in stripped)


def plugin_script_path(content_root: Path, plugin_name: str) -> Path | None:
    """返回范围内存在的活动插件脚本；配置名不能改变目标目录。"""

    if (
        not plugin_name
        or plugin_name.strip() != plugin_name
        or "/" in plugin_name
        or "\\" in plugin_name
        or ":" in plugin_name
        or plugin_name in {".", ".."}
    ):
        fail(
            "js/plugins.js",
            "活动插件 name 不是单个自然文件名",
            "修正插件配置，不要在 name 中使用空白、盘符或目录分隔符",
        )
    path = content_root / "js" / "plugins" / f"{plugin_name}.js"
    if not path.exists():
        return None
    return require_file_within(path, content_root, f"活动插件 {plugin_name} 的脚本")


def discover_game(path: Path) -> GameInfo:
    supplied = require_directory(path, "游戏目录")
    candidates = [supplied]
    www = supplied / "www"
    if www.exists():
        candidates.append(ensure_inside(www, supplied, "www 内容目录"))
    valid: list[Path] = []
    for candidate in candidates:
        marker = candidate / "data" / "System.json"
        if marker.exists():
            require_file_within(marker, candidate, "data/System.json")
            valid.append(candidate)
    if not valid:
        fail(
            str(supplied),
            "没有找到 data/System.json",
            "指定 RPG Maker 游戏根或包含 data 与 js 的实际内容根",
        )
    if len(valid) > 1:
        relative_candidates = ", ".join(
            "." if candidate == supplied else candidate.relative_to(supplied).as_posix()
            for candidate in valid
        )
        fail(
            str(supplied),
            f"同时找到多个可能的内容根：{relative_candidates}",
            "核对 Patch/MOD 的实际运行入口，并把其中一个精确内容根直接传给 --game",
        )
    content = valid[0]
    mv_marker = content / "js" / "rpg_core.js"
    mz_marker = content / "js" / "rmmz_core.js"
    mv_exists = mv_marker.exists()
    mz_exists = mz_marker.exists()
    if mv_exists:
        require_file_within(mv_marker, content, "MV 核心脚本")
    if mz_exists:
        require_file_within(mz_marker, content, "MZ 核心脚本")
    if mv_exists and mz_exists:
        fail(str(content / "js"), "同时存在 MV 和 MZ 核心脚本", "确认正在调查的实际内容根")
    if mz_exists:
        engine = "mz"
    elif mv_exists:
        engine = "mv"
    else:
        fail(
            str(content / "js"),
            "缺少 rpg_core.js 与 rmmz_core.js，无法确认 MV 或 MZ",
            "使用包含权威核心脚本的未损坏游戏内容根；不要用插件描述或参数猜测引擎",
        )
    return GameInfo(supplied_root=supplied, content_root=content, engine=engine)


def _extract_json_array(text: str, object_name: str) -> list[JsonValue]:
    assignment = _PLUGINS_ASSIGNMENT.search(text)
    if assignment is None:
        fail(
            object_name,
            "plugins.js 中没有找到 $plugins 赋值",
            "检查 plugins.js 是否为完整的 RPG Maker 插件配置",
        )
    start = text.find("[", assignment.end())
    if start < 0:
        fail(object_name, "plugins.js 中没有找到插件数组", "检查 plugins.js 是否为完整的 RPG Maker 插件配置")
    raw, _ = parse_json_prefix(text[start:], f"{object_name} 中的 $plugins 数组")
    if not isinstance(raw, list):
        fail(object_name, "plugins.js 中的 $plugins 不是 array", "恢复 RPG Maker 生成的插件数组")
    return raw


def read_plugins(content_root: Path) -> list[PluginInfo]:
    path = content_root / "js" / "plugins.js"
    if not path.exists():
        fail(str(path), "缺少 RPG Maker plugins.js", "恢复实际内容根中的完整插件配置后重试")
    source = require_file_within(path, content_root, "plugins.js")
    entries = _extract_json_array(source.read_text(encoding="utf-8-sig"), str(source))
    result: list[PluginInfo] = []
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            fail(str(path), f"插件数组第 {index + 1} 项不是 object", "修正 plugins.js 中的插件项")
        name = entry.get("name")
        status = entry.get("status")
        description = entry.get("description", "")
        parameters = entry.get("parameters", {})
        if not isinstance(name, str) or not isinstance(status, bool):
            fail(str(path), f"插件数组第 {index + 1} 项缺少有效 name/status", "修正该插件的名称和启用状态")
        if not isinstance(description, str) or not isinstance(parameters, dict):
            fail(str(path), f"插件 {name} 的 description/parameters 类型无效", "修正该插件配置")
        typed_parameters: dict[str, JsonValue] = {}
        for key, value in parameters.items():
            typed_parameters[key] = value
        result.append(
            PluginInfo(
                name=name,
                status=status,
                description=description,
                parameters=typed_parameters,
                index=index,
            )
        )
    return result


def canonical_map_files(data_root: Path) -> list[Path]:
    result: list[tuple[int, Path]] = []
    resolved_data = require_directory(data_root, "data 目录")
    for path in safe_walk_files(resolved_data):
        if path.parent != resolved_data:
            continue
        match = _MAP_NAME.fullmatch(path.name)
        if match is None:
            continue
        digits = match.group(1)
        value = int(digits)
        if value < 1 or value > 4_294_967_295:
            continue
        expected = f"{value:03d}" if value < 1000 else str(value)
        if digits != expected:
            continue
        result.append((value, path))
    return [path for _, path in sorted(result)]


def is_builtin_data_path(file_name: str, path: tuple[str | int, ...], *, canonical_map: bool) -> bool:
    """判断一个字符串路径是否已由 Builtin 或事件命令扫描拥有。"""

    fields = BUILTIN_DATABASE_FIELDS.get(file_name)
    if fields is not None:
        return len(path) == 2 and isinstance(path[0], int) and path[1] in fields
    if file_name == "System.json":
        if len(path) == 1 and path[0] in {"gameTitle", "currencyUnit"}:
            return True
        if len(path) == 2 and path[0] in _SYSTEM_ARRAYS and isinstance(path[1], int):
            return True
        if len(path) == 3 and path[0] == "terms" and path[1] in {"basic", "commands", "params"}:
            return isinstance(path[2], int)
        if len(path) == 3 and path[0] == "terms" and path[1] == "messages":
            return isinstance(path[2], str)
    if canonical_map:
        if path == ("displayName",):
            return True
        return (
            len(path) >= 7
            and path[0] == "events"
            and isinstance(path[1], int)
            and path[2] == "pages"
            and isinstance(path[3], int)
            and path[4] == "list"
            and isinstance(path[5], int)
            and path[6] == "parameters"
        )
    if file_name == "CommonEvents.json":
        return (
            len(path) >= 4
            and isinstance(path[0], int)
            and path[1] == "list"
            and isinstance(path[2], int)
            and path[3] == "parameters"
        )
    if file_name == "Troops.json":
        return (
            len(path) >= 6
            and isinstance(path[0], int)
            and path[1] == "pages"
            and isinstance(path[2], int)
            and path[3] == "list"
            and isinstance(path[4], int)
            and path[5] == "parameters"
        )
    return False


def _command_list(value: JsonValue, source_file: str, prefix: str) -> Iterator[EventCommand]:
    if not isinstance(value, list):
        fail(source_file, f"{prefix} 的事件列表不是 array", "修正游戏数据中的事件列表")
    for command_index, command in enumerate(value):
        if not isinstance(command, dict):
            fail(
                source_file,
                f"{prefix} 第 {command_index + 1} 条事件命令不是 object",
                "修正游戏数据中的事件命令",
            )
        code = command.get("code")
        if not isinstance(code, int) or isinstance(code, bool):
            fail(
                source_file,
                f"{prefix} 第 {command_index + 1} 条事件命令缺少有效 code",
                "把 code 修正为整数",
            )
        parameters = command.get("parameters")
        if not isinstance(parameters, list):
            fail(
                source_file,
                f"{prefix} 第 {command_index + 1} 条事件命令 parameters 不是 array",
                "修正该事件命令参数",
            )
        yield EventCommand(
            source_file=source_file,
            location=f"{prefix}:command{command_index + 1}",
            index=command_index,
            code=code,
            parameters=parameters,
        )


def iter_event_commands(content_root: Path) -> Iterator[EventCommand]:
    data = content_root / "data"
    for path in canonical_map_files(data):
        root = read_json(path, "Map JSON", allowed_root=content_root)
        if not isinstance(root, dict):
            fail(str(path), "Map JSON 根值不是 object", "修正 Map JSON")
        events = root.get("events")
        if not isinstance(events, list):
            fail(str(path), "events 不是 array", "修正 Map JSON 的 events")
        for event_index, event in enumerate(events):
            if event is None:
                continue
            if not isinstance(event, dict):
                fail(str(path), f"events[{event_index}] 不是 object", "修正该地图事件")
            pages = event.get("pages")
            if not isinstance(pages, list):
                fail(str(path), f"event{event_index} 的 pages 不是 array", "修正该地图事件")
            for page_index, page in enumerate(pages):
                if not isinstance(page, dict):
                    fail(str(path), f"event{event_index}:page{page_index + 1} 不是 object", "修正该事件页")
                yield from _command_list(
                    page.get("list"),
                    path.name,
                    f"{path.name}:event{event_index}:page{page_index + 1}",
                )
    common_path = data / "CommonEvents.json"
    if common_path.exists():
        common = read_json(common_path, "CommonEvents.json", allowed_root=content_root)
        if not isinstance(common, list):
            fail(str(common_path), "CommonEvents.json 根值不是 array", "修正 CommonEvents.json")
        for event_index, event in enumerate(common):
            if event is None:
                continue
            if not isinstance(event, dict):
                fail(str(common_path), f"第 {event_index} 项不是 object", "修正 CommonEvents.json")
            yield from _command_list(
                event.get("list"),
                common_path.name,
                f"CommonEvents.json:event{event_index}",
            )
    troops_path = data / "Troops.json"
    if troops_path.exists():
        troops = read_json(troops_path, "Troops.json", allowed_root=content_root)
        if not isinstance(troops, list):
            fail(str(troops_path), "Troops.json 根值不是 array", "修正 Troops.json")
        for troop_index, troop in enumerate(troops):
            if troop is None:
                continue
            if not isinstance(troop, dict):
                fail(str(troops_path), f"第 {troop_index} 项不是 object", "修正 Troops.json")
            pages = troop.get("pages")
            if not isinstance(pages, list):
                fail(str(troops_path), f"troop{troop_index} 的 pages 不是 array", "修正该敌群事件")
            for page_index, page in enumerate(pages):
                if not isinstance(page, dict):
                    fail(
                        str(troops_path),
                        f"troop{troop_index}:page{page_index + 1} 不是 object",
                        "修正该敌群事件",
                    )
                yield from _command_list(
                    page.get("list"),
                    troops_path.name,
                    f"Troops.json:troop{troop_index}:page{page_index + 1}",
                )


def iter_dialogue_first_lines(content_root: Path) -> Iterator[tuple[EventCommand, str]]:
    commands = list(iter_event_commands(content_root))
    for index, command in enumerate(commands):
        if command.code != 101 or index + 1 >= len(commands):
            continue
        following = commands[index + 1]
        is_first_body = (
            following.source_file == command.source_file
            and following.location.rsplit(":command", 1)[0] == command.location.rsplit(":command", 1)[0]
            and following.index == command.index + 1
            and following.code == 401
        )
        if not is_first_body:
            continue
        if not following.parameters or not isinstance(following.parameters[0], str):
            fail(
                following.source_file,
                f"{following.location} 的 401 正文参数缺失或不是 string",
                "修正损坏的 RPG Maker 对话事件命令",
            )
        yield command, following.parameters[0]


def _looks_like_encoded_json(value: str) -> bool:
    stripped = value.strip()
    if not stripped:
        return False
    if stripped[0] == '"':
        # 普通正文经常整句使用 ASCII 引号。只有出现 JSON string 的转义引号，
        # 才把这一层视为“又编码了一层 JSON”的候选；成功解码后仍会再次确认
        # 内层确实还是 JSON，而不是擅自去掉正文引号。
        return len(stripped) > 1 and stripped[-1] == '"' and '\\"' in stripped
    if stripped[0] == "{":
        remainder = stripped[1:].lstrip()
        if stripped[-1] == "}":
            return not remainder or remainder[0] in {'"', "}"}
        # 缺少右花括号仍可能是损坏的序列化对象。要求先出现完整的 JSON key
        # 与冒号，避免把事件脚本中的单独“{”误报成损坏数据。
        if not remainder.startswith('"'):
            return False
        escaped = False
        for index, character in enumerate(remainder[1:], start=1):
            if escaped:
                escaped = False
                continue
            if character == "\\":
                escaped = True
                continue
            if character == '"':
                return remainder[index + 1 :].lstrip().startswith(":")
        return False
    if stripped[0] == "[":
        try:
            _, end = json.JSONDecoder().raw_decode(stripped)
        except json.JSONDecodeError:
            # 玩家文本常用一层或多层方括号作标签，例如
            # ``[-Gallery-]``、``[13] ...`` 和 ``[[[Before Menu]]]``。
            # 只有剥去数组起始括号后仍出现明确的 JSON 值起始符，才把
            # 失败视为损坏的序列化数组；否则保留为普通文本。
            remainder = stripped
            while remainder.startswith("["):
                remainder = remainder[1:].lstrip()
            if not remainder:
                return False
            if remainder[0] in {'"', "{", "]"} or remainder[0].isdigit():
                return True
            if remainder[0] == "-" and len(remainder) > 1 and remainder[1].isdigit():
                return True
            for literal in ("true", "false", "null", "NaN", "Infinity", "-Infinity"):
                if not remainder.startswith(literal):
                    continue
                following = remainder[len(literal) : len(literal) + 1]
                return not following or following.isspace() or following in {",", "]", "}"}
            return False
        # 完整数组后若还有正文，它是玩家文本中的方括号片段，不是序列化数据。
        return not stripped[end:].strip()
    return False


def _try_decode(value: str, object_name: str) -> JsonValue | None:
    if not _looks_like_encoded_json(value):
        return None
    decoded = parse_json_text(value, object_name)
    if isinstance(decoded, str) and (decoded == value or not _looks_like_encoded_json(decoded)):
        return None
    return decoded


def iter_string_leaves(
    value: JsonValue,
    *,
    path: tuple[str | int, ...] = (),
    decoded_layers: int = 0,
) -> Iterator[StringLeaf]:
    if isinstance(value, str):
        current = value
        layers = decoded_layers
        seen: set[str] = set()
        while True:
            if current in seen:
                fail(
                    actual_path(path) or "JSON string",
                    "嵌套 JSON string 解码没有推进",
                    "检查循环或损坏的编码层",
                )
            seen.add(current)
            decoded = _try_decode(current, actual_path(path) or "嵌套 JSON string")
            if decoded is None:
                yield StringLeaf(path=path, value=current, decoded_layers=layers)
                return
            layers += 1
            if isinstance(decoded, str):
                current = decoded
                continue
            yield from iter_string_leaves(decoded, path=path, decoded_layers=layers)
            return
    elif isinstance(value, list):
        for index, item in enumerate(value):
            if item is not None:
                yield from iter_string_leaves(
                    item,
                    path=(*path, index),
                    decoded_layers=decoded_layers,
                )
    elif isinstance(value, dict):
        for key, item in value.items():
            yield from iter_string_leaves(
                item,
                path=(*path, key),
                decoded_layers=decoded_layers,
            )


def normalized_path(path: tuple[str | int, ...]) -> str:
    result = ""
    for step in path:
        if isinstance(step, int):
            result += "[]"
        elif _BARE_KEY.fullmatch(step):
            result += ("." if result else "") + step
        else:
            result += f"[{json.dumps(step, ensure_ascii=False)}]"
    return result


def actual_path(path: tuple[str | int, ...]) -> str:
    result = ""
    for step in path:
        if isinstance(step, int):
            result += f"[{step}]"
        elif _BARE_KEY.fullmatch(step):
            result += ("." if result else "") + step
        else:
            result += f"[{json.dumps(step, ensure_ascii=False)}]"
    return result


def load_data_json(path: Path, content_root: Path) -> JsonValue:
    return read_json(path, "RPG Maker data JSON", allowed_root=content_root)


def type_name(value: JsonValue) -> str:
    return json_type(value)


def mapping_value(mapping: Mapping[str, JsonValue], key: str) -> JsonValue | None:
    return mapping.get(key)
