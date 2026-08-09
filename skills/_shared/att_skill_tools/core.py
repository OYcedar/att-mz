"""ATT Skill 程序共用的参数、文件、JSON、TOML 与 Manual 边界。"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tomllib
import unicodedata
from collections import deque
from collections.abc import Callable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import NoReturn, TypeAlias, cast

JsonScalar: TypeAlias = None | bool | int | float | str
JsonValue: TypeAlias = JsonScalar | Sequence["JsonValue"] | Mapping[str, "JsonValue"]


@dataclass(slots=True)
class ToolError(Exception):
    object_name: str
    reason: str
    impact: str
    help_text: str

    def __str__(self) -> str:
        return self.reason


@dataclass(frozen=True, slots=True)
class ManualEntry:
    readable_id: str
    translation_type: str
    source: tuple[str, ...]
    translation: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class TermOccurrence:
    count: int
    locations: tuple[tuple[str, int], ...]


class _StrictJsonError(ValueError):
    pass


def sanitize_line(value: object) -> str:
    """移除会伪造终端行、方向或控制序列的字符。"""

    text = str(value)
    cleaned: list[str] = []
    for character in text:
        category = unicodedata.category(character)
        if category in {"Cc", "Cf", "Zl", "Zp"} or character == "\u0085":
            cleaned.append(" ")
        else:
            cleaned.append(character)
    return " ".join("".join(cleaned).split()).strip() or "未提供"


def display_path(path: Path) -> str:
    return sanitize_line(path.resolve(strict=False))


def _os_reason(error: OSError) -> str:
    return sanitize_line(error.strerror or type(error).__name__)


def _failure_text(error: BaseException) -> str:
    if isinstance(error, ToolError):
        return sanitize_line(error.reason)
    if isinstance(error, KeyboardInterrupt):
        return "使用者取消了命令"
    if isinstance(error, OSError):
        return _os_reason(error)
    if isinstance(error, UnicodeError):
        return f"文本编码无效（{type(error).__name__}）"
    return f"未预期内部错误（{type(error).__name__}）"


def fail(object_name: str, reason: str, help_text: str) -> NoReturn:
    raise ToolError(
        object_name=object_name,
        reason=reason,
        impact="没有写入目标输出，输入原件没有修改",
        help_text=help_text,
    )


def print_error(error: ToolError) -> None:
    print("错误：", file=sys.stderr)
    print(f"对象：{sanitize_line(error.object_name)}", file=sys.stderr)
    print(f"原因：{sanitize_line(error.reason)}", file=sys.stderr)
    print(f"影响：{sanitize_line(error.impact)}", file=sys.stderr)
    print(f"处理办法：{sanitize_line(error.help_text)}", file=sys.stderr)


class ToolArgumentParser(argparse.ArgumentParser):
    def error(self, message: str) -> NoReturn:
        print_error(
            ToolError(
                object_name="命令行参数",
                reason=f"参数无效：{message}",
                impact="工具没有运行，输入原件没有修改",
                help_text=f"运行 {self.prog} --help 查看必填参数和用法",
            )
        )
        self.exit(2)


def run_cli(main: Callable[[], int]) -> None:
    try:
        status = main()
    except ToolError as error:
        print_error(error)
        raise SystemExit(1) from None
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        object_name = getattr(error, "filename", None) or "输入或输出文件"
        reason = (
            _os_reason(error) if isinstance(error, OSError) else f"文本或 JSON 无效（{type(error).__name__}）"
        )
        print_error(
            ToolError(
                object_name=str(object_name),
                reason=reason,
                impact="本次处理没有完成；无法确认显式输出，输入原件没有修改",
                help_text="检查自然路径、编码、文件格式和目录权限后重试",
            )
        )
        raise SystemExit(1) from None
    except KeyboardInterrupt:
        print_error(
            ToolError(
                object_name="当前工具命令",
                reason="使用者取消了命令",
                impact="本次输出没有完成，输入原件没有修改",
                help_text="确认是否需要重新运行命令",
            )
        )
        raise SystemExit(130) from None
    except Exception as error:  # noqa: BLE001 - 进程边界不能泄漏 traceback 或吞掉致命错误。
        print_error(
            ToolError(
                object_name="当前工具命令",
                reason=f"工具发生未预期的内部错误（{type(error).__name__}）",
                impact="无法确认本次显式输出是否已经建立；输入原件不会由本工具修改",
                help_text="保留现有输出并报告脚本、参数和输入；确认前不要把本次运行视为成功",
            )
        )
        raise SystemExit(1) from None
    raise SystemExit(status)


def _is_reparse_point(path: Path) -> bool:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return False
    except OSError:
        fail(str(path), "路径无法读取元数据", "移除损坏链接或恢复路径后重试")
    attributes = getattr(metadata, "st_file_attributes", 0)
    return path.is_symlink() or bool(attributes & 0x400)


def _reject_reparse_chain(path: Path, root: Path, description: str) -> None:
    lexical_root = root.absolute()
    lexical_path = path.absolute()
    if _is_reparse_point(lexical_root):
        fail(str(root), f"{description}的允许根是链接或重解析点", "传入真实内容所在的精确普通目录")
    try:
        relative = lexical_path.relative_to(lexical_root)
    except ValueError:
        return
    current = lexical_root
    for part in relative.parts:
        current /= part
        if _is_reparse_point(current):
            fail(
                str(current),
                f"{description}经过链接或重解析点",
                "移除链接，或把链接目标作为精确来源单独调查",
            )


def require_file(path: Path, description: str) -> Path:
    if _is_reparse_point(path):
        fail(str(path), f"{description}是链接或重解析点", "传入真实普通文件的精确路径")
    try:
        resolved = path.resolve(strict=True)
    except OSError:
        fail(str(path), f"找不到{description}", "提供存在的文件绝对路径")
    if not resolved.is_file():
        fail(str(path), f"找不到{description}", "提供存在的文件绝对路径")
    return resolved


def require_directory(path: Path, description: str) -> Path:
    if _is_reparse_point(path):
        fail(str(path), f"{description}是链接或重解析点", "传入真实普通目录的精确路径")
    try:
        resolved = path.resolve(strict=True)
    except OSError:
        fail(str(path), f"找不到{description}", "提供存在的目录绝对路径")
    if not resolved.is_dir():
        fail(str(path), f"找不到{description}", "提供存在的目录绝对路径")
    return resolved


def _is_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


def ensure_inside(path: Path, root: Path, description: str) -> Path:
    _reject_reparse_chain(path, root, description)
    resolved_root = root.resolve(strict=True)
    try:
        resolved = path.resolve(strict=True)
    except OSError:
        fail(str(path), f"找不到{description}", "提供存在的精确路径")
    if not _is_within(resolved, resolved_root):
        fail(str(path), f"{description}解析后位于允许范围之外", "不要读取指向游戏目录外的链接或重解析点")
    return resolved


def require_file_within(path: Path, root: Path, description: str) -> Path:
    resolved = ensure_inside(path, root, description)
    if not resolved.is_file():
        fail(str(path), f"{description}不是普通文件", "提供范围内存在的普通文件")
    return resolved


def stable_relative(path: Path, root: Path) -> str:
    resolved = ensure_inside(path, root, "路径")
    return resolved.relative_to(root.resolve(strict=True)).as_posix()


def safe_walk_files(root: Path) -> Iterator[Path]:
    """完整枚举普通文件；任何链接或重解析点都明确失败。"""

    if _is_reparse_point(root):
        fail(str(root), "扫描根是链接或重解析点", "传入真实内容所在的精确普通目录")
    resolved_root = require_directory(root, "扫描根目录")

    def walk_error(error: OSError) -> NoReturn:
        fail(
            str(error.filename or resolved_root),
            f"递归扫描失败：{_os_reason(error)}",
            "修正该目录的权限或文件系统错误后重新完整扫描",
        )

    for current_text, directory_names, file_names in os.walk(
        resolved_root,
        followlinks=False,
        onerror=walk_error,
    ):
        current = Path(current_text)
        for name in list(directory_names):
            candidate = current / name
            if _is_reparse_point(candidate):
                fail(
                    str(candidate),
                    "扫描目录包含链接或重解析点，无法证明来源已完整盘点",
                    "移除链接，或把链接目标作为精确内容根单独调查",
                )
            try:
                resolved = candidate.resolve(strict=True)
            except OSError:
                fail(str(candidate), "扫描目录无法解析", "移除损坏链接或恢复目录后重试")
            if not _is_within(resolved, resolved_root):
                fail(str(candidate), "目录链接指向游戏根之外", "移除该链接，或把外部来源单独显式调查")
        for name in file_names:
            candidate = current / name
            if _is_reparse_point(candidate):
                fail(
                    str(candidate),
                    "扫描文件是链接或重解析点，无法保留唯一自然来源",
                    "移除链接，或把链接目标作为精确来源单独调查",
                )
            yield require_file_within(candidate, resolved_root, "扫描文件")


def protect_outputs(
    outputs: Sequence[Path],
    *,
    inputs: Sequence[Path] = (),
    forbidden_roots: Sequence[Path] = (),
    replace: bool,
) -> None:
    """拒绝覆盖输入、写进游戏树或用目录替换吞掉输入。"""

    resolved_outputs = [path.resolve(strict=False) for path in outputs]
    if len(set(resolved_outputs)) != len(resolved_outputs):
        fail("输出路径", "同一次命令的多个输出指向同一位置", "为不同输出使用不同路径")
    for index, output in enumerate(resolved_outputs):
        for other in resolved_outputs[index + 1 :]:
            if _is_within(output, other) or _is_within(other, output):
                fail("输出路径", "同一次命令的输出路径互相包含", "为每个输出使用互不包含的独立路径")
    resolved_inputs = [path.resolve(strict=True) for path in inputs]
    roots = [path.resolve(strict=True) for path in forbidden_roots]
    for output in resolved_outputs:
        for root in roots:
            if output == root or _is_within(output, root):
                fail(str(output), f"输出位于受保护目录 {root} 中", "把输出放到游戏和输入目录之外的工作目录")
        for input_path in resolved_inputs:
            input_is_directory = input_path.is_dir()
            if (
                output == input_path
                or _is_within(input_path, output)
                or (input_is_directory and _is_within(output, input_path))
            ):
                fail(str(output), f"输出与输入 {input_path} 重叠", "为输出使用不包含任何输入的独立路径")
        if output.exists() and not replace:
            fail(str(output), "目标已存在", "换一个输出路径，或在确认可替换后传入 --replace")


def preflight_atomic_text_outputs(outputs: Sequence[Path], *, replace: bool) -> None:
    """在多文件命令写入前检查每个固定目标和临时文件。"""

    for output in outputs:
        target = output.resolve(strict=False)
        temporary = target.with_name(f".{target.name}.tmp")
        if target.exists() and not replace:
            fail(str(target), "目标已存在", "换一个输出路径，或在确认可替换后传入 --replace")
        if temporary.exists():
            fail(str(temporary), "存在上次未清理的固定临时文件", "检查并处理该 .tmp 文件后重试")
        ancestor = target.parent
        while not ancestor.exists() and ancestor != ancestor.parent:
            ancestor = ancestor.parent
        if not ancestor.is_dir():
            fail(str(ancestor), "输出父路径不是目录", "为输出选择可建立子文件的目录")


def _strict_object(pairs: list[tuple[str, JsonValue]]) -> dict[str, JsonValue]:
    result: dict[str, JsonValue] = {}
    for key, value in pairs:
        if key in result:
            raise _StrictJsonError(f"重复 key：{key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> NoReturn:
    raise _StrictJsonError(f"非有限数字：{value}")


def parse_json_prefix(text: str, object_name: str) -> tuple[JsonValue, int]:
    try:
        start = len(text) - len(text.lstrip(" \t\r\n"))
        return cast(
            tuple[JsonValue, int],
            json.JSONDecoder(
                object_pairs_hook=_strict_object,
                parse_constant=_reject_constant,
            ).raw_decode(text, start),
        )
    except json.JSONDecodeError as error:
        fail(
            object_name,
            f"JSON 语法错误：第 {error.lineno} 行第 {error.colno} 列，{error.msg}",
            "修正 JSON 后重试",
        )
    except _StrictJsonError as error:
        fail(object_name, str(error), "删除重复 key 或非有限数字后重试")


def parse_json_text(text: str, object_name: str) -> JsonValue:
    try:
        value, end = parse_json_prefix(text, object_name)
        if text[end:].strip():
            raise json.JSONDecodeError("JSON 根值后存在额外内容", text, end)
        return value
    except json.JSONDecodeError as error:
        fail(
            object_name,
            f"JSON 语法错误：第 {error.lineno} 行第 {error.colno} 列，{error.msg}",
            "修正 JSON 后重试",
        )
    except _StrictJsonError as error:
        fail(object_name, str(error), "删除重复 key 或非有限数字后重试")


def read_json(path: Path, description: str = "JSON 文件", *, allowed_root: Path | None = None) -> JsonValue:
    source = (
        require_file(path, description)
        if allowed_root is None
        else require_file_within(path, allowed_root, description)
    )
    return parse_json_text(source.read_text(encoding="utf-8-sig"), str(source))


def read_json_object(
    path: Path, description: str = "JSON 文件", *, allowed_root: Path | None = None
) -> dict[str, JsonValue]:
    value = read_json(path, description, allowed_root=allowed_root)
    if not isinstance(value, dict):
        fail(str(path), f"{description}的根值不是 object", "提供以 object 为根的 JSON")
    return value


def _remove_temporary(path: Path) -> BaseException | None:
    try:
        path.unlink(missing_ok=True)
    except BaseException as error:  # noqa: BLE001 - 清理失败不能覆盖主失败或取消。
        return error
    return None


def _raise_file_failure(
    primary: BaseException,
    cleanup: BaseException | None,
    *,
    target: Path,
    temporary: Path,
) -> NoReturn:
    if cleanup is None:
        raise ToolError(
            object_name=str(target),
            reason=f"目标写入或发布失败：{_failure_text(primary)}",
            impact="目标没有发布，固定临时文件已经清理；输入原件没有修改",
            help_text="检查目标目录权限、占用和剩余空间后重试",
        ) from None
    raise ToolError(
        object_name=str(target),
        reason=f"目标写入失败：{_failure_text(primary)}；固定临时文件清理也失败：{_failure_text(cleanup)}",
        impact=f"目标没有发布；临时文件 {temporary} 仍可能存在；输入原件没有修改",
        help_text="保留并检查指出的 .tmp 文件，处理占用或权限后再重新运行",
    ) from None


def atomic_write_text(path: Path, text: str, *, replace: bool) -> None:
    target = path.resolve(strict=False)
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(f".{target.name}.tmp")
    if target.exists() and not replace:
        fail(str(target), "目标已存在", "换一个输出路径，或在确认可替换后传入 --replace")
    if temporary.exists():
        fail(str(temporary), "存在上次未清理的固定临时文件", "检查并处理该 .tmp 文件后重试")
    created = False
    try:
        with temporary.open("x", encoding="utf-8", newline="\n") as handle:
            created = True
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
    except BaseException as primary:  # noqa: BLE001 - 所有失败都必须清理已经建立的临时文件。
        _raise_file_failure(
            primary,
            _remove_temporary(temporary) if created else None,
            target=target,
            temporary=temporary,
        )
    try:
        if replace:
            os.replace(temporary, target)
        else:
            os.link(temporary, target)
    except FileExistsError as primary:
        cleanup = _remove_temporary(temporary)
        if cleanup is None:
            fail(str(target), "目标在写入期间已由其他进程建立", "保留该文件并重新选择输出路径")
        _raise_file_failure(primary, cleanup, target=target, temporary=temporary)
    except BaseException as primary:  # noqa: BLE001 - 发布失败必须保留主失败与清理结果。
        _raise_file_failure(primary, _remove_temporary(temporary), target=target, temporary=temporary)
    if replace:
        return
    cleanup = _remove_temporary(temporary)
    if cleanup is not None:
        raise ToolError(
            object_name=str(temporary),
            reason=f"目标已经完整建立，但固定临时文件清理失败：{_failure_text(cleanup)}",
            impact=f"目标 {target} 已经生效；输入原件没有修改",
            help_text="保留目标，处理占用或权限后删除指出的 .tmp 文件",
        )


def write_json(path: Path, value: JsonValue, *, replace: bool) -> None:
    atomic_write_text(
        path,
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        replace=replace,
    )


def write_json_with_optional_text(
    json_path: Path,
    value: JsonValue,
    *,
    text_path: Path | None,
    text: str | None,
    replace: bool,
) -> None:
    """发布候选 JSON，并准确保留随后可选文本输出的结果。"""

    if (text_path is None) != (text is None):
        fail("工具输出", "可选文本的路径和正文没有同时提供", "报告当前脚本实现错误")
    outputs = [json_path] if text_path is None else [json_path, text_path]
    preflight_atomic_text_outputs(outputs, replace=replace)
    write_json(json_path, value, replace=replace)
    if text_path is None or text is None:
        return
    try:
        atomic_write_text(text_path, text, replace=replace)
    except ToolError as error:
        raise ToolError(
            object_name=error.object_name,
            reason=error.reason,
            impact=f"候选 JSON {json_path.resolve(strict=False)} 已经生效；{error.impact}",
            help_text=error.help_text,
        ) from None


def _remove_tree(path: Path) -> BaseException | None:
    try:
        shutil.rmtree(path)
    except BaseException as error:  # noqa: BLE001 - 清理结果必须与主失败分开保存。
        return error
    return None


def _raise_directory_cleanup_failure(
    primary: BaseException,
    cleanup: BaseException,
    *,
    target: Path,
    stage: Path,
    restored: bool,
) -> NoReturn:
    restored_text = "原目标已经恢复" if restored else "目标没有由本工具发布"
    raise ToolError(
        object_name=str(target),
        reason=f"目录写入或发布失败：{_failure_text(primary)}；临时目录清理也失败：{_failure_text(cleanup)}",
        impact=f"{restored_text}；临时目录 {stage} 仍可能存在；输入原件没有修改",
        help_text="保留并检查指出的 .tmp 目录，处理占用或权限后再重新运行",
    ) from None


def atomic_write_directory(
    target_path: Path, files: Mapping[str, str | bytes | Path], *, replace: bool
) -> None:
    """原子发布由 UTF-8 文本、原始字节或普通源文件组成的目录。"""

    target = target_path.resolve(strict=False)
    target.parent.mkdir(parents=True, exist_ok=True)
    stage = target.with_name(f".{target.name}.tmp")
    previous = target.with_name(f".{target.name}.previous")
    if target.exists() and not replace:
        fail(str(target), "目标目录已存在", "换一个输出目录，或在确认可替换后传入 --replace")
    if stage.exists() or previous.exists():
        fail(str(target.parent), "存在上次未清理的固定临时目录", "检查并处理 .tmp/.previous 目录后重试")
    stage.mkdir()
    try:
        for relative_text, body in sorted(files.items()):
            relative = PurePosixPath(relative_text)
            if (
                not relative.parts
                or relative == PurePosixPath(".")
                or relative.is_absolute()
                or ".." in relative.parts
                or "\\" in relative_text
                or ":" in relative_text
            ):
                fail(relative_text, "输出文件名超出目标目录", "使用目标目录内的相对路径")
            destination = stage.joinpath(*relative.parts).resolve(strict=False)
            if not _is_within(destination, stage.resolve(strict=True)):
                fail(relative_text, "输出文件名解析后超出目标目录", "使用不含盘符、根路径和反斜杠的相对路径")
            destination.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(body, Path):
                source = require_file(body, "目录输出源文件")
                with source.open("rb") as source_handle, destination.open("xb") as handle:
                    shutil.copyfileobj(source_handle, handle, length=1024 * 1024)
                    handle.flush()
                    os.fsync(handle.fileno())
                continue
            if isinstance(body, bytes):
                with destination.open("xb") as handle:
                    handle.write(body)
                    handle.flush()
                    os.fsync(handle.fileno())
                continue
            with destination.open("x", encoding="utf-8", newline="\n") as handle:
                handle.write(body)
                handle.flush()
                os.fsync(handle.fileno())
    except BaseException as primary:
        cleanup = _remove_tree(stage)
        if cleanup is not None:
            _raise_directory_cleanup_failure(primary, cleanup, target=target, stage=stage, restored=False)
        raise

    moved_previous = False
    if target.exists():
        try:
            os.replace(target, previous)
            moved_previous = True
        except BaseException as primary:
            cleanup = _remove_tree(stage)
            if cleanup is not None:
                _raise_directory_cleanup_failure(primary, cleanup, target=target, stage=stage, restored=True)
            raise
    try:
        if replace:
            os.replace(stage, target)
        else:
            os.rename(stage, target)
    except BaseException as primary:
        if moved_previous:
            try:
                os.replace(previous, target)
            except BaseException as rollback:  # noqa: BLE001 - 恢复失败决定结果未知。
                raise ToolError(
                    object_name=str(target.parent),
                    reason=f"新目录发布失败：{_failure_text(primary)}；恢复旧目录也失败：{_failure_text(rollback)}",
                    impact="无法确认目标目录状态；完整 .tmp/.previous 现场已经保留",
                    help_text="不要删除或重试；先检查目标、.tmp 和 .previous 三个自然路径并恢复旧目录",
                ) from None
        cleanup = _remove_tree(stage)
        if cleanup is not None:
            _raise_directory_cleanup_failure(
                primary, cleanup, target=target, stage=stage, restored=moved_previous
            )
        raise
    if moved_previous:
        cleanup = _remove_tree(previous)
        if cleanup is not None:
            raise ToolError(
                object_name=str(previous),
                reason=f"新目录已经发布，但旧目录清理失败：{_failure_text(cleanup)}",
                impact=f"新目录 {target} 已经生效；旧目录仍保留在指出的位置",
                help_text="保留新目录，处理占用或权限后删除 .previous 目录",
            )


def toml_string(value: str) -> str:
    # TOML 字面字符串不解释反斜杠，适合规则、Placeholder 和术语中的
    # PCRE2、JSON path 与游戏控制符。含单引号或控制字符时退回 basic
    # string，避免生成无法无损表示的字面字符串。
    if "'" not in value and all(character >= " " and character != "\x7f" for character in value):
        return f"'{value}'"
    return json.dumps(value, ensure_ascii=False).replace("\x7f", "\\u007F")


def json_type(value: JsonValue) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, str):
        return "string"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, list):
        return "array"
    return "object"


def require_list(value: JsonValue, object_name: str, field: str) -> list[JsonValue]:
    if not isinstance(value, list):
        fail(object_name, f"{field} 必须是 array", f"把 {field} 改为 JSON array")
    return value


def require_string(value: JsonValue, object_name: str, field: str) -> str:
    if not isinstance(value, str) or value == "":
        fail(object_name, f"{field} 必须是非空 string", f"为 {field} 提供非空字符串")
    return value


def strings_only(values: Sequence[JsonValue], object_name: str, field: str) -> list[str]:
    result: list[str] = []
    for index, value in enumerate(values, start=1):
        if not isinstance(value, str):
            fail(object_name, f"{field} 第 {index} 项不是 string", f"把 {field} 中的每项都改为字符串")
        result.append(value)
    return result


def validate_object_keys(value: Mapping[str, JsonValue], object_name: str, allowed: set[str]) -> None:
    unknown = sorted(set(value) - allowed)
    if unknown:
        fail(object_name, f"存在未知字段：{', '.join(unknown)}", f"只保留：{', '.join(sorted(allowed))}")


def _string_array(value: object, path: Path, entry_number: int, field: str) -> tuple[str, ...]:
    if not isinstance(value, list):
        fail(str(path), f"第 {entry_number} 项 {field} 不是 string array", "重新运行 manual export")
    result: list[str] = []
    for item in cast(list[object], value):
        if not isinstance(item, str) or any(character in item for character in ("\r", "\n", "\x00")):
            fail(
                str(path),
                f"第 {entry_number} 项 {field} 不是无控制字符的 string array",
                "重新运行 manual export",
            )
        result.append(item)
    return tuple(result)


def read_manual(path: Path) -> list[ManualEntry]:
    source_path = require_file(path, "Manual TOML")
    try:
        root = cast(dict[str, object], tomllib.loads(source_path.read_text(encoding="utf-8-sig")))
    except tomllib.TOMLDecodeError as error:
        fail(str(source_path), f"Manual TOML 语法错误：{error}", "修正 TOML 后重试")
    if set(root) - {"translation"}:
        fail(str(source_path), "Manual TOML 根存在 translation 之外的字段", "使用当前 manual export 格式")
    raw_entries = root.get("translation", [])
    if not isinstance(raw_entries, list):
        fail(str(source_path), "translation 不是 array of tables", "使用 [[translation]]")
    result: list[ManualEntry] = []
    seen: set[str] = set()
    for number, raw in enumerate(cast(list[object], raw_entries), start=1):
        if not isinstance(raw, dict):
            fail(str(source_path), f"第 {number} 项 translation 不是 table", "重新运行 manual export")
        item = cast(dict[str, object], raw)
        if set(item) != {"id", "type", "source", "translation"}:
            fail(str(source_path), f"第 {number} 项字段不符合 Manual 当前格式", "重新运行 manual export")
        readable_id = item.get("id")
        translation_type = item.get("type")
        if (
            not isinstance(readable_id, str)
            or not readable_id
            or any(character in readable_id for character in ("\r", "\n", "\x00"))
            or readable_id in seen
        ):
            fail(str(source_path), f"第 {number} 项 id 为空、含控制字符或重复", "重新运行 manual export")
        if not isinstance(translation_type, str) or translation_type not in {"fixed", "free"}:
            fail(str(source_path), f"{readable_id} 的 type 不是 fixed 或 free", "重新运行 manual export")
        seen.add(readable_id)
        result.append(
            ManualEntry(
                readable_id=readable_id,
                translation_type=translation_type,
                source=_string_array(item.get("source"), source_path, number, "source"),
                translation=_string_array(item.get("translation"), source_path, number, "translation"),
            )
        )
    return result


def scan_term_occurrences(terms: Sequence[str], entries: Sequence[ManualEntry]) -> dict[str, TermOccurrence]:
    """用 Aho–Corasick 一次扫描语料，保留候选之间的重叠命中。"""

    if not terms:
        return {}
    if any(not term for term in terms) or len(set(terms)) != len(terms):
        fail("Formic 候选", "多模式扫描收到空值或重复候选", "先删除空值并按原值去重")
    transitions: list[dict[str, int]] = [{}]
    failures = [0]
    outputs: list[list[int]] = [[]]
    for pattern_index, term in enumerate(terms):
        state = 0
        for character in term:
            next_state = transitions[state].get(character)
            if next_state is None:
                next_state = len(transitions)
                transitions[state][character] = next_state
                transitions.append({})
                failures.append(0)
                outputs.append([])
            state = next_state
        outputs[state].append(pattern_index)
    queue: deque[int] = deque(transitions[0].values())
    while queue:
        state = queue.popleft()
        for character, next_state in transitions[state].items():
            queue.append(next_state)
            fallback = failures[state]
            while fallback and character not in transitions[fallback]:
                fallback = failures[fallback]
            failures[next_state] = transitions[fallback].get(character, 0)
            outputs[next_state].extend(outputs[failures[next_state]])
    totals = [0] * len(terms)
    locations: list[list[tuple[str, int]]] = [[] for _ in terms]
    for entry in entries:
        entry_counts: dict[int, int] = {}
        for line in entry.source:
            state = 0
            for character in line:
                while state and character not in transitions[state]:
                    state = failures[state]
                state = transitions[state].get(character, 0)
                for pattern_index in outputs[state]:
                    entry_counts[pattern_index] = entry_counts.get(pattern_index, 0) + 1
        for pattern_index, count in entry_counts.items():
            totals[pattern_index] += count
            locations[pattern_index].append((entry.readable_id, count))
    return {
        term: TermOccurrence(count=totals[index], locations=tuple(locations[index]))
        for index, term in enumerate(terms)
    }
