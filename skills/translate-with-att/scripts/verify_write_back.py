#!/usr/bin/env python3
"""在 WriteBack 前后比较游戏关键源文件，并检查输出 JSON 与文本变化。"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path, PurePosixPath

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

from att_skill_tools import (
    JsonValue,
    ToolArgumentParser,
    ToolError,
    atomic_write_directory,
    display_path,
    fail,
    protect_outputs,
    read_json,
    read_json_object,
    require_directory,
    require_file_within,
    run_cli,
    safe_walk_files,
    write_json,
)
from att_toolbox.rpg import discover_game


def _parser() -> argparse.ArgumentParser:
    parser = ToolArgumentParser(description="确认 WriteBack 输出可读，且游戏原件未被修改。")
    subparsers = parser.add_subparsers(dest="command", required=True)
    snapshot = subparsers.add_parser("snapshot", help="WriteBack 前保存关键源文件工作副本")
    snapshot.add_argument("--game", type=Path, required=True)
    snapshot.add_argument("--output", type=Path, required=True, help="baseline 工作副本目录")
    snapshot.add_argument("--replace", action="store_true")
    verify = subparsers.add_parser("verify", help="WriteBack 后验证源文件和输出")
    verify.add_argument("--game", type=Path, required=True)
    verify.add_argument("--output-root", type=Path, required=True)
    verify.add_argument("--baseline", type=Path, required=True)
    verify.add_argument("--report", type=Path, required=True)
    verify.add_argument("--replace", action="store_true")
    return parser


def _same_bytes(left: Path, right: Path) -> bool:
    if left.stat().st_size != right.stat().st_size:
        return False
    with left.open("rb") as left_handle, right.open("rb") as right_handle:
        while True:
            left_block = left_handle.read(1024 * 1024)
            right_block = right_handle.read(1024 * 1024)
            if left_block != right_block:
                return False
            if not left_block:
                return True


def _key_files(content_root: Path) -> list[Path]:
    data_root = (content_root / "data").resolve(strict=True)
    result = [path for path in safe_walk_files(data_root) if path.suffix.lower() == ".json"]
    for relative in ("js/plugins.js", "js/rpg_core.js", "js/rmmz_core.js"):
        path = content_root / relative
        if path.exists():
            result.append(require_file_within(path, content_root, relative))
    return sorted(result, key=lambda item: item.relative_to(content_root).as_posix().encode("utf-8"))


def _snapshot(args: argparse.Namespace) -> int:
    game = discover_game(args.game)
    protect_outputs(
        [args.output],
        forbidden_roots=[game.supplied_root, game.content_root],
        replace=args.replace,
    )
    key_files = _key_files(game.content_root)
    manifest_files: list[JsonValue] = []
    baseline_files: dict[str, str | Path] = {}
    for path in key_files:
        relative = path.relative_to(game.content_root).as_posix()
        manifest_files.append(
            {
                "path": relative,
                "bytes": path.stat().st_size,
            }
        )
        baseline_files[f"files/{relative}"] = path
    result: dict[str, JsonValue] = {
        "content_root": str(game.content_root),
        "engine": game.engine,
        "files": manifest_files,
    }
    baseline_files["baseline.json"] = json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    atomic_write_directory(args.output, baseline_files, replace=args.replace)
    print(f"已保存 {len(key_files)} 个关键源文件的 WriteBack 前工作副本：{display_path(args.output)}")
    return 0


def _baseline_relative(value: JsonValue, manifest: Path, number: int) -> PurePosixPath:
    if not isinstance(value, str):
        fail(str(manifest), f"files 第 {number} 项缺少 path", "重新运行 snapshot")
    relative = PurePosixPath(value)
    if (
        not relative.parts
        or relative == PurePosixPath(".")
        or relative.is_absolute()
        or ".." in relative.parts
        or "\\" in value
        or ":" in value
    ):
        fail(str(manifest), f"files 第 {number} 项 path 不是自然相对路径", "重新运行 snapshot")
    return relative


def _baseline(path: Path) -> tuple[Path, str, dict[str, Path]]:
    baseline_root = require_directory(path, "WriteBack 前工作副本目录")
    manifest = require_file_within(baseline_root / "baseline.json", baseline_root, "baseline.json")
    root = read_json_object(manifest, "WriteBack 前工作副本清单", allowed_root=baseline_root)
    if set(root) != {"content_root", "engine", "files"}:
        fail(str(manifest), "baseline 根字段不符合当前格式", "重新运行 snapshot")
    content_root = root.get("content_root")
    engine = root.get("engine")
    if not isinstance(content_root, str) or not isinstance(engine, str) or engine not in {"mv", "mz"}:
        fail(str(manifest), "baseline 缺少有效 content_root/engine", "重新运行 snapshot")
    raw_files = root.get("files")
    if not isinstance(raw_files, list):
        fail(str(manifest), "baseline 缺少 files array", "重新运行 snapshot")
    result: dict[str, Path] = {}
    for number, raw in enumerate(raw_files, start=1):
        if not isinstance(raw, dict):
            fail(str(manifest), f"files 第 {number} 项不是 object", "重新运行 snapshot")
        if set(raw) != {"path", "bytes"}:
            fail(str(manifest), f"files 第 {number} 项字段不符合当前格式", "重新运行 snapshot")
        relative_path = _baseline_relative(raw.get("path"), manifest, number)
        relative = relative_path.as_posix()
        size = raw.get("bytes")
        if not isinstance(size, int) or isinstance(size, bool) or size < 0:
            fail(str(manifest), f"files 第 {number} 项缺少有效 bytes", "重新运行 snapshot")
        if relative in result:
            fail(str(manifest), f"baseline 重复文件 {relative}", "重新运行 snapshot")
        baseline_file = require_file_within(
            baseline_root.joinpath("files", *relative_path.parts),
            baseline_root,
            f"源文件工作副本 {relative}",
        )
        if baseline_file.stat().st_size != size:
            fail(str(baseline_file), "工作副本大小与 baseline.json 不一致", "重新运行 snapshot")
        result[relative] = baseline_file
    files_root = require_directory(baseline_root / "files", "源文件工作副本目录")
    actual = {item.relative_to(files_root).as_posix() for item in safe_walk_files(files_root)}
    if actual != set(result):
        fail(str(files_root), "工作副本文件集合与 baseline.json 不一致", "重新运行 snapshot")
    return Path(content_root).resolve(), engine, result


def _json_child(path: str, key: str | int) -> str:
    if isinstance(key, int):
        return f"{path}[{key}]"
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key):
        return f"{path}.{key}"
    return f"{path}[{json.dumps(key, ensure_ascii=False)}]"


def _value_type(value: JsonValue) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    return "object"


def _native_event_list_path(path: str) -> bool:
    return bool(
        re.fullmatch(r"data/Map[0-9]+\.json\.events\[[0-9]+\]\.pages\[[0-9]+\]\.list", path)
        or re.fullmatch(r"data/CommonEvents\.json\[[0-9]+\]\.list", path)
        or re.fullmatch(r"data/Troops\.json\[[0-9]+\]\.pages\[[0-9]+\]\.list", path)
    )


def _event_command_code(value: JsonValue) -> int | None:
    if not isinstance(value, dict) or not isinstance(value.get("parameters"), list):
        return None
    code = value.get("code")
    if not isinstance(code, int) or isinstance(code, bool):
        return None
    return code


def _is_event_command_list(value: list[JsonValue]) -> bool:
    return bool(value) and all(_event_command_code(command) is not None for command in value)


def _event_chunks(values: list[JsonValue]) -> list[tuple[int, int, list[JsonValue]]]:
    chunks: list[tuple[int, int, list[JsonValue]]] = []
    index = 0
    while index < len(values):
        code = _event_command_code(values[index])
        assert code is not None
        body_code = 401 if code == 101 else 405 if code == 105 else None
        end = index + 1
        if body_code is not None:
            while end < len(values) and _event_command_code(values[end]) == body_code:
                end += 1
        chunks.append((code, index, values[index:end]))
        index = end
    return chunks


def _event_body_template(value: JsonValue) -> JsonValue | None:
    if not isinstance(value, dict):
        return None
    parameters = value.get("parameters")
    if not isinstance(parameters, list) or not parameters or not isinstance(parameters[0], str):
        return None
    template = dict(value)
    template["parameters"] = ["", *parameters[1:]]
    return template


def _match_event_body_templates(source: list[JsonValue], output: list[JsonValue]) -> list[JsonValue] | None:
    if source and not output:
        return None
    source_templates = [_event_body_template(command) for command in source]
    matched: list[JsonValue] = []
    source_index = 0
    for output_command in output:
        output_template = _event_body_template(output_command)
        while source_index < len(source_templates) and source_templates[source_index] != output_template:
            source_index += 1
        if source_index == len(source_templates) or output_template is None:
            return None
        matched.append(source[source_index])
    return matched


def _compare_event_lists(
    source: list[JsonValue],
    output: list[JsonValue],
    *,
    path: str,
) -> tuple[int, int, int, list[JsonValue]]:
    source_chunks = _event_chunks(source)
    output_chunks = _event_chunks(output)
    if len(source_chunks) != len(output_chunks) or any(
        source_chunk[0] != output_chunk[0]
        for source_chunk, output_chunk in zip(source_chunks, output_chunks, strict=False)
    ):
        return 0, 0, max(1, abs(len(source_chunks) - len(output_chunks))), []

    same = changed = structural = 0
    non_text: list[JsonValue] = []
    for source_chunk, output_chunk in zip(source_chunks, output_chunks, strict=True):
        code, _, source_commands = source_chunk
        _, output_start, output_commands = output_chunk
        header = _compare_values(
            source_commands[0],
            output_commands[0],
            path=_json_child(path, output_start),
        )
        same += header[0]
        changed += header[1]
        structural += header[2]
        non_text.extend(header[3])
        if code not in {101, 105}:
            continue
        matched = _match_event_body_templates(source_commands[1:], output_commands[1:])
        if matched is None:
            structural += max(1, len(output_commands) - 1)
            continue
        for offset, (source_command, output_command) in enumerate(
            zip(matched, output_commands[1:], strict=True), start=1
        ):
            current = _compare_values(
                source_command,
                output_command,
                path=_json_child(path, output_start + offset),
            )
            same += current[0]
            changed += current[1]
            structural += current[2]
            non_text.extend(current[3])
    return same, changed, structural, non_text


def _compare_values(
    source: JsonValue,
    output: JsonValue,
    *,
    path: str,
) -> tuple[int, int, int, list[JsonValue]]:
    """返回字符串计数、结构差异和非文本值变化的自然位置。"""

    if isinstance(source, str) and isinstance(output, str):
        return (1, 0, 0, []) if source == output else (0, 1, 0, [])
    if isinstance(source, list) and isinstance(output, list):
        if (
            _native_event_list_path(path)
            and _is_event_command_list(source)
            and _is_event_command_list(output)
        ):
            return _compare_event_lists(source, output, path=path)
        same = changed = structural = 0
        non_text: list[JsonValue] = []
        for index, (left, right) in enumerate(zip(source, output, strict=False)):
            current = _compare_values(left, right, path=_json_child(path, index))
            same += current[0]
            changed += current[1]
            structural += current[2]
            non_text.extend(current[3])
        structural += abs(len(source) - len(output))
        return same, changed, structural, non_text
    if isinstance(source, dict) and isinstance(output, dict):
        same = changed = 0
        structural = len(set(source) ^ set(output))
        non_text = []
        for key in sorted(set(source) & set(output)):
            current = _compare_values(source[key], output[key], path=_json_child(path, key))
            same += current[0]
            changed += current[1]
            structural += current[2]
            non_text.extend(current[3])
        return same, changed, structural, non_text
    source_scalar = source is None or (isinstance(source, (bool, int, float)) and not isinstance(source, str))
    output_scalar = output is None or (isinstance(output, (bool, int, float)) and not isinstance(output, str))
    if source_scalar and output_scalar and (type(source) is not type(output) or source != output):
        return (
            0,
            0,
            0,
            [{"path": path, "source_type": _value_type(source), "output_type": _value_type(output)}],
        )
    if type(source) is not type(output):
        return 0, 0, 1, []
    return 0, 0, 0, []


def _verify(args: argparse.Namespace) -> int:
    game = discover_game(args.game)
    output_game = discover_game(args.output_root)
    protect_outputs(
        [args.report],
        inputs=[args.baseline],
        forbidden_roots=[
            game.supplied_root,
            game.content_root,
            output_game.supplied_root,
            output_game.content_root,
        ],
        replace=args.replace,
    )
    try:
        args.output_root.resolve().relative_to(game.supplied_root)
    except ValueError:
        pass
    else:
        fail(
            str(args.output_root.resolve()),
            "WriteBack 输出位于游戏原件目录中",
            "使用游戏目录之外的独立输出目录",
        )
    if game.engine != output_game.engine:
        fail(str(output_game.content_root), "WriteBack 输出引擎标记与源游戏不一致", "检查是否选错输出目录")

    baseline_root, baseline_engine, expected = _baseline(args.baseline)
    if baseline_root != game.content_root or baseline_engine != game.engine:
        fail(str(args.baseline), "baseline 不属于当前游戏内容根和引擎", "对当前游戏重新运行 snapshot")
    current = {path.relative_to(game.content_root).as_posix(): path for path in _key_files(game.content_root)}
    changed_source = sorted(
        path for path in expected if path in current and not _same_bytes(current[path], expected[path])
    )
    missing_source = sorted(set(expected) - set(current))
    added_source = sorted(set(current) - set(expected))

    source_data = game.content_root / "data"
    output_data = output_game.content_root / "data"
    source_json_files = [path for path in safe_walk_files(source_data) if path.suffix.lower() == ".json"]
    output_json_files = [path for path in safe_walk_files(output_data) if path.suffix.lower() == ".json"]
    invalid_json: list[JsonValue] = []
    missing_output: list[str] = []
    same_strings = changed_strings = structural_changes = 0
    non_text_changes: list[JsonValue] = []
    for source_path in sorted(
        source_json_files,
        key=lambda item: item.relative_to(source_data).as_posix().encode("utf-8"),
    ):
        relative = source_path.relative_to(source_data)
        natural_path = f"data/{relative.as_posix()}"
        output_path = output_data / relative
        if not output_path.exists():
            missing_output.append(natural_path)
            continue
        output_path = require_file_within(output_path, output_game.content_root, "WriteBack JSON")
        try:
            source_json = read_json(source_path, allowed_root=game.content_root)
            output_json = read_json(output_path, allowed_root=output_game.content_root)
        except ToolError as error:
            invalid_json.append({"path": natural_path, "reason": error.reason})
            continue
        comparison = _compare_values(source_json, output_json, path=natural_path)
        same_strings += comparison[0]
        changed_strings += comparison[1]
        structural_changes += comparison[2]
        non_text_changes.extend(comparison[3])

    expected_output_files = set(expected)
    output_key_files = {
        path.relative_to(output_game.content_root).as_posix(): path
        for path in _key_files(output_game.content_root)
    }
    missing_output.extend(sorted(expected_output_files - set(output_key_files)))
    source_data_names = {path.relative_to(source_data).as_posix() for path in source_json_files}
    added_output_data = sorted(
        f"data/{path.relative_to(output_data).as_posix()}"
        for path in output_json_files
        if path.relative_to(output_data).as_posix() not in source_data_names
    )
    unchanged_core_failures: list[str] = []
    for relative in ("js/rpg_core.js", "js/rmmz_core.js"):
        output_path = output_key_files.get(relative)
        if (
            relative in expected
            and output_path is not None
            and not _same_bytes(output_path, expected[relative])
        ):
            unchanged_core_failures.append(relative)

    report: dict[str, JsonValue] = {
        "source_content_root": str(game.content_root),
        "output_content_root": str(output_game.content_root),
        "source_unchanged": not (changed_source or missing_source or added_source),
        "changed_source_files": changed_source,
        "missing_source_files": missing_source,
        "added_source_key_files": added_source,
        "output_json_valid": not invalid_json,
        "invalid_output_json": invalid_json,
        "missing_output_key_files": sorted(set(missing_output)),
        "added_output_data_files": added_output_data,
        "changed_output_core_files": unchanged_core_failures,
        "string_values": {"translated_or_changed": changed_strings, "unchanged": same_strings},
        "structural_differences": structural_changes,
        "non_text_value_changes": non_text_changes,
    }
    write_json(args.report, report, replace=args.replace)
    problems = (
        len(changed_source)
        + len(missing_source)
        + len(added_source)
        + len(invalid_json)
        + len(set(missing_output))
        + len(added_output_data)
        + len(unchanged_core_failures)
        + structural_changes
        + len(non_text_changes)
    )
    print(
        f"WriteBack 验证：变更字符串 {changed_strings}，未变字符串 {same_strings}，"
        f"结构差异 {structural_changes}，非文本值变化 {len(non_text_changes)}，问题 {problems}。"
    )
    print(f"验证报告：{display_path(args.report)}")
    if problems:
        raise ToolError(
            object_name=str(args.report.resolve()),
            reason=f"发现 {problems} 个源文件或输出文件问题",
            impact="验证报告已写入，当前 WriteBack 输出不能通过验收",
            help_text="按报告中的自然路径检查源文件变化、缺失文件和无效 JSON，不要覆盖游戏原件",
        )
    return 0


def _main(args: argparse.Namespace) -> int:
    if args.command == "snapshot":
        return _snapshot(args)
    if args.command == "verify":
        return _verify(args)
    fail("命令行", "缺少 snapshot 或 verify", "选择一个子命令")


if __name__ == "__main__":
    parsed = _parser().parse_args()
    run_cli(lambda: _main(parsed))
