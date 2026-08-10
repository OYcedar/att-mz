"""调查文件读取、来源冻结与严格文本解码。"""

from __future__ import annotations

import hashlib
from collections.abc import Mapping
from pathlib import Path
from typing import cast

from att_skill_tools import (
    JsonValue,
    fail,
    parse_json_text,
    read_json_object,
    require_directory,
    require_file,
    safe_walk_files,
    validate_object_keys,
)

from .survey_model import FileSnapshot


def file_bytes(path: Path, game_root: Path) -> tuple[bytes, FileSnapshot]:
    raw = path.read_bytes()
    relative = path.relative_to(game_root).as_posix()
    return raw, FileSnapshot(relative, len(raw), hashlib.sha256(raw).hexdigest())


def decode_text(raw: bytes, path: Path) -> str:
    try:
        return raw.decode("utf-8-sig")
    except UnicodeDecodeError as error:
        fail(
            str(path),
            f"文本不是有效 UTF-8：字节位置 {error.start}",
            "确认文件编码和实际运行时读取方式；不要用替换字符继续分析",
        )


def read_jsonl(path: Path, description: str) -> list[dict[str, JsonValue]]:
    """读取机器管理的 JSONL，并拒绝空行或非 object 行。"""

    source = require_file(path, description)
    output: list[dict[str, JsonValue]] = []
    for line_number, line in enumerate(source.read_text(encoding="utf-8-sig").splitlines(), start=1):
        if not line.strip():
            fail(str(source), f"第 {line_number} 行为空", "重新生成该机器管理文件")
        value = parse_json_text(line, f"{source} 第 {line_number} 行")
        if not isinstance(value, dict):
            fail(str(source), f"第 {line_number} 行不是 object", "重新生成该机器管理文件")
        output.append(dict(value))
    return output


def load_survey(
    path: Path,
) -> tuple[
    dict[str, JsonValue], list[dict[str, JsonValue]], list[dict[str, JsonValue]], dict[str, JsonValue]
]:
    """读取一次 scan 的五个权威文件，并核对摘要计数。"""

    root = require_directory(path, "survey 作业目录")
    survey = read_json_object(root / "survey.json", "survey.json", allowed_root=root)
    locations = read_jsonl(root / "locations.jsonl", "locations.jsonl")
    groups = read_jsonl(root / "review-groups.jsonl", "review-groups.jsonl")
    baseline = read_json_object(root / "source-baseline.json", "source-baseline.json", allowed_root=root)
    if survey.get("locations") != len(locations) or survey.get("review_groups") != len(groups):
        fail(str(root), "survey 计数与明细不一致", "不要手工改写机器管理文件；重新执行 scan")
    return survey, locations, groups, baseline


def verify_source_baseline(survey: Mapping[str, JsonValue], baseline: Mapping[str, JsonValue]) -> None:
    """只按 scan 保存的字节数和摘要确认游戏来源未变，不重复解析。"""

    game_root_value = survey.get("game_root")
    files_value = baseline.get("files")
    selection_value = baseline.get("selection")
    if (
        not isinstance(game_root_value, str)
        or not isinstance(files_value, list)
        or not isinstance(selection_value, dict)
    ):
        fail("source-baseline.json", "缺少游戏根、文件清单或来源选择范围", "使用当前工具重新执行 scan")
    game_root = require_directory(Path(game_root_value), "survey 记录的游戏根")
    selection = selection_value
    validate_object_keys(
        selection,
        "source-baseline.selection",
        {"data_directory", "plugins_file", "external_suffixes", "paths"},
    )
    data_directory = selection.get("data_directory")
    plugins_file = selection.get("plugins_file")
    suffixes_value = selection.get("external_suffixes")
    paths_value = selection.get("paths")
    if (
        not isinstance(data_directory, str)
        or not isinstance(plugins_file, str)
        or not isinstance(suffixes_value, list)
        or not isinstance(paths_value, list)
        or any(not isinstance(value, str) or not value for value in suffixes_value)
        or any(not isinstance(value, str) or not value for value in paths_value)
    ):
        fail("source-baseline.json", "来源选择范围字段无效", "使用当前工具重新执行 scan")
    suffixes = set(cast(list[str], suffixes_value))
    expected_paths = set(cast(list[str], paths_value))
    if len(expected_paths) != len(paths_value):
        fail("source-baseline.json", "来源选择路径重复", "使用当前工具重新执行 scan")
    data_root = game_root.joinpath(*data_directory.split("/")).resolve(strict=True)
    current_paths = {
        path.relative_to(game_root).as_posix()
        for path in safe_walk_files(game_root)
        if path.relative_to(game_root).as_posix() == plugins_file
        or (path.parent == data_root and path.suffix.lower() == ".json")
        or path.suffix.lower() in suffixes
    }
    if current_paths != expected_paths:
        added = sorted(current_paths - expected_paths)
        removed = sorted(expected_paths - current_paths)
        detail = f"新增 {added[0]}" if added else f"缺少 {removed[0]}"
        fail(
            str(game_root),
            f"来源选择范围与 scan 时不同：{detail}",
            "停止沿用旧决定；对当前游戏重新执行 scan",
        )
    for number, raw_item in enumerate(cast(list[object], files_value), start=1):
        if not isinstance(raw_item, dict):
            fail("source-baseline.json", f"第 {number} 项不是 object", "使用当前工具重新执行 scan")
        item = cast(dict[str, JsonValue], raw_item)
        validate_object_keys(item, f"source-baseline 第 {number} 项", {"path", "bytes", "sha256"})
        relative = item.get("path")
        expected_bytes = item.get("bytes")
        expected_digest = item.get("sha256")
        if (
            not isinstance(relative, str)
            or not isinstance(expected_bytes, int)
            or not isinstance(expected_digest, str)
        ):
            fail("source-baseline.json", f"第 {number} 项字段类型无效", "使用当前工具重新执行 scan")
        source = require_file(game_root.joinpath(*relative.split("/")), f"survey 来源 {relative}")
        raw = source.read_bytes()
        if len(raw) != expected_bytes or hashlib.sha256(raw).hexdigest() != expected_digest:
            fail(str(source), "来源字节与 scan 时不同", "停止沿用旧决定；对当前游戏重新执行 scan")
