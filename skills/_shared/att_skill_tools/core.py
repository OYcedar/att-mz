"""ATT Skill 程序共用的参数、文件、JSON、TOML 与 Manual 边界。"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import stat
import sys
import tomllib
import unicodedata
from collections import deque
from collections.abc import Callable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Literal, NoReturn, TypeAlias, cast

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


@dataclass(slots=True)
class OutputPublishedError(ToolError):
    """输出已经完整发布，但发布或后续清理调用以错误或取消结束。"""

    cause: BaseException


@dataclass(slots=True)
class ToolCancelledError(ToolError):
    """命令已经取消，并携带取消期间发现的清理事实。"""

    cause: KeyboardInterrupt


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
    return sanitize_line(error.strerror or str(error) or type(error).__name__)


def _failure_text(error: BaseException) -> str:
    if isinstance(error, ToolError):
        return sanitize_line(error.reason)
    if isinstance(error, KeyboardInterrupt):
        detail = str(error).strip()
        return f"使用者取消了命令：{sanitize_line(detail)}" if detail else "使用者取消了命令"
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
    except ToolCancelledError as error:
        print_error(error)
        raise SystemExit(130) from None
    except OutputPublishedError as error:
        print_error(error)
        raise SystemExit(130 if isinstance(error.cause, KeyboardInterrupt) else 1) from None
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


def print_published_completion(
    message: str,
    *,
    object_name: str,
    impact: str | None = None,
    help_text: str = "直接查看已发布结果并继续后续步骤，无需重新运行当前命令",
) -> None:
    """输出最终完成提示；输出失败时保留已经完成的业务事实。"""

    try:
        print(message, flush=True)
    except (OSError, UnicodeError, ValueError, KeyboardInterrupt) as error:
        details = {
            "object_name": object_name,
            "reason": f"结果已经完成，但最终完成提示输出失败：{_failure_text(error)}",
            "impact": impact or f"{object_name} 已经完整发布；最终完成提示未能显示",
            "help_text": help_text,
        }
        if isinstance(error, KeyboardInterrupt):
            raise ToolCancelledError(**details, cause=error) from None
        raise ToolError(**details) from None


def _is_reparse_point(path: Path) -> bool:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return False
    except OSError:
        fail(str(path), "路径无法读取元数据", "移除损坏链接或恢复路径后重试")
    attributes = getattr(metadata, "st_file_attributes", 0)
    return path.is_symlink() or bool(attributes & 0x400)


def _lexical_absolute(path: Path) -> Path:
    """建立绝对自然路径，不解析链接或重解析点。"""

    return Path(os.path.abspath(path))


def _output_target(path: Path, description: str) -> Path:
    """返回输出的自然绝对路径，并拒绝现有重解析链。"""

    target = _lexical_absolute(path)
    current = target
    while True:
        if _is_reparse_point(current):
            location = "本身是" if current == target else "经过"
            fail(
                str(current),
                f"{description}{location}链接或重解析点",
                "为输出选择由普通目录组成的自然路径",
            )
        parent = current.parent
        if parent == current:
            return target
        current = parent


def _reject_hard_link(path: Path, description: str) -> None:
    try:
        link_count = path.lstat().st_nlink
    except OSError:
        fail(str(path), f"{description}无法读取元数据", "恢复文件或目录权限后重新完整扫描")
    if link_count != 1:
        fail(
            str(path),
            f"{description}是硬链接，无法证明唯一物理来源",
            "为扫描范围中的每个逻辑路径使用独立普通文件",
        )


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
            _reject_hard_link(candidate, "扫描文件")
            yield require_file_within(candidate, resolved_root, "扫描文件")


def protect_outputs(
    outputs: Sequence[Path],
    *,
    inputs: Sequence[Path] = (),
    forbidden_roots: Sequence[Path] = (),
    replace: bool,
) -> None:
    """拒绝覆盖输入、写进游戏树或用目录替换吞掉输入。"""

    lexical_outputs = [_output_target(path, "输出路径") for path in outputs]
    comparison_outputs = [path.resolve(strict=False) for path in lexical_outputs]
    if len(set(comparison_outputs)) != len(comparison_outputs):
        fail("输出路径", "同一次命令的多个输出指向同一位置", "为不同输出使用不同路径")
    for index, output in enumerate(comparison_outputs):
        for other in comparison_outputs[index + 1 :]:
            if _is_within(output, other) or _is_within(other, output):
                fail("输出路径", "同一次命令的输出路径互相包含", "为每个输出使用互不包含的独立路径")
    resolved_inputs = [path.resolve(strict=True) for path in inputs]
    roots = [path.resolve(strict=True) for path in forbidden_roots]
    for lexical_output, output in zip(lexical_outputs, comparison_outputs, strict=True):
        for root in roots:
            if output == root or _is_within(output, root) or _is_within(root, output):
                fail(
                    str(lexical_output),
                    f"输出与受保护目录 {root} 重叠",
                    "把输出放到游戏和输入目录之外的独立工作目录",
                )
        for input_path in resolved_inputs:
            if output == input_path or _is_within(input_path, output) or _is_within(output, input_path):
                fail(
                    str(lexical_output),
                    f"输出与输入 {input_path} 重叠",
                    "为输出使用不包含任何输入的独立路径",
                )
        if lexical_output.exists() and not replace:
            fail(str(lexical_output), "目标已存在", "换一个输出路径，或在确认可替换后传入 --replace")


def preflight_atomic_text_outputs(outputs: Sequence[Path], *, replace: bool) -> None:
    """在多文件命令写入前检查每个固定目标和临时文件。"""

    for output in outputs:
        target = _output_target(output, "输出路径")
        temporary = target.with_name(f".{target.name}.tmp")
        _output_target(temporary, "固定临时文件")
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


def _finite_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed):
        raise _StrictJsonError("JSON number 超出本工具可表示的有限范围")
    return parsed


def parse_json_prefix(text: str, object_name: str) -> tuple[JsonValue, int]:
    try:
        start = len(text) - len(text.lstrip(" \t\r\n"))
        return cast(
            tuple[JsonValue, int],
            json.JSONDecoder(
                object_pairs_hook=_strict_object,
                parse_constant=_reject_constant,
                parse_float=_finite_float,
            ).raw_decode(text, start),
        )
    except json.JSONDecodeError as error:
        fail(
            object_name,
            f"JSON 语法错误：第 {error.lineno} 行第 {error.colno} 列，{error.msg}",
            "修正 JSON 后重试",
        )
    except _StrictJsonError as error:
        fail(object_name, str(error), "修正重复 key，或把数字改为本工具可表示的有限 JSON number")


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
        fail(object_name, str(error), "修正重复 key，或把数字改为本工具可表示的有限 JSON number")


def physical_jsonl_lines(text: str, object_name: str) -> Iterator[tuple[int, str]]:
    """按物理 LF/CRLF 行枚举 JSONL 正文，并保留自然行号。"""

    start = 0
    line_number = 1
    while start < len(text):
        end = text.find("\n", start)
        if end == -1:
            line = text[start:]
            next_start = len(text)
        else:
            line = text[start:end]
            next_start = end + 1
        if end != -1:
            line = line.removesuffix("\r")
        if "\r" in line:
            fail(
                object_name,
                f"第 {line_number} 行包含未与 LF 配对的 CR",
                "使用 LF 或 CRLF 保存每条物理 JSONL 行",
            )
        yield line_number, line
        if end == -1:
            return
        start = next_start
        line_number += 1


def read_physical_text(path: Path, *, encoding: str = "utf-8-sig") -> str:
    """读取文本并保留原始换行序列，供物理行协议自行校验。"""

    with path.open("r", encoding=encoding, newline="") as handle:
        return handle.read()


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


_FileIdentity = tuple[int, int]


class _ForeignFileTypeError(OSError):
    """路径元数据可读取，但对象不是普通文件。"""


def _ordinary_file_identity(path: Path) -> _FileIdentity:
    metadata = path.lstat()
    file_attributes = getattr(metadata, "st_file_attributes", 0)
    if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or bool(file_attributes & 0x400):
        raise _ForeignFileTypeError(f"{path} 不是普通文件")
    if metadata.st_ino == 0:
        raise OSError(f"{path} 所在文件系统没有提供稳定文件身份")
    return metadata.st_dev, metadata.st_ino


def _file_identity_at(path: Path) -> tuple[_FileIdentity | None, BaseException | None]:
    try:
        return _ordinary_file_identity(path), None
    except FileNotFoundError:
        return None, None
    except BaseException as error:  # noqa: BLE001 - 发布与清理终态必须保留无法确认的文件事实。
        return None, error


def _candidate_file_cleanup(
    path: Path,
    expected_identity: _FileIdentity | None,
) -> tuple[BaseException | None, tuple[str, ...]]:
    """只删除身份匹配的候选文件，并返回清理调用与实际残留位置。"""

    actual_identity, inspection_error = _file_identity_at(path)
    if inspection_error is not None:
        return inspection_error, (f"{path}（状态无法确认：{type(inspection_error).__name__}）",)
    if actual_identity is None:
        return None, ()
    if expected_identity is None or actual_identity != expected_identity:
        return OSError(f"固定临时文件身份已经变化，已保留于 {path}"), (str(path),)
    try:
        path.unlink()
    except BaseException as error:  # noqa: BLE001 - unlink 返回异常后仍以后验身份判断实际结果。
        remaining_identity, remaining_error = _file_identity_at(path)
        if remaining_error is not None:
            return error, (f"{path}（状态无法确认：{type(remaining_error).__name__}）",)
        if remaining_identity is None:
            return error, ()
        return error, (str(path),)
    return None, ()


def _file_publish_state(
    temporary: Path,
    target: Path,
    expected_identity: _FileIdentity,
) -> tuple[
    Literal["published", "not_published", "unknown"],
    tuple[str, ...],
    KeyboardInterrupt | None,
]:
    temporary_identity, temporary_error = _file_identity_at(temporary)
    target_identity, target_error = _file_identity_at(target)
    facts: list[str] = []
    if temporary_error is not None:
        label = "不是普通文件" if isinstance(temporary_error, _ForeignFileTypeError) else "状态无法读取"
        facts.append(f"固定临时文件 {temporary} {label}：{_failure_text(temporary_error)}")
    if target_error is not None:
        label = "不是普通文件" if isinstance(target_error, _ForeignFileTypeError) else "状态无法读取"
        facts.append(f"目标文件 {target} {label}：{_failure_text(target_error)}")
    if target_error is None and target_identity == expected_identity:
        if temporary_error is None and temporary_identity not in {None, expected_identity}:
            facts.append(f"固定临时路径已由其他文件占用，保留于 {temporary}")
        return "published", tuple(facts), _cancellation_from(temporary_error, target_error)
    if temporary_identity == expected_identity and (
        target_error is None or isinstance(target_error, _ForeignFileTypeError)
    ):
        return "not_published", tuple(facts), _cancellation_from(temporary_error, target_error)
    if facts:
        return "unknown", tuple(facts), _cancellation_from(temporary_error, target_error)
    if temporary_identity is not None:
        facts.append(f"固定临时路径已由其他文件占用，保留于 {temporary}")
    return "unknown", tuple(facts), _cancellation_from(temporary_error, target_error)


def _cancellation_from(*errors: BaseException | None) -> KeyboardInterrupt | None:
    return next((error for error in errors if isinstance(error, KeyboardInterrupt)), None)


def _raise_file_failure(
    primary: BaseException,
    cleanup: BaseException | None,
    *,
    target: Path,
    retained_sites: tuple[str, ...] = (),
    facts: tuple[str, ...] = (),
) -> NoReturn:
    reason = f"目标写入或发布失败：{_failure_text(primary)}"
    if facts:
        reason += f"；{'；'.join(facts)}"
    if cleanup is not None:
        reason += f"；固定临时文件清理调用发生异常：{_failure_text(cleanup)}"
    if retained_sites:
        impact = f"目标没有发布；临时文件保留于或需确认于 {' 与 '.join(retained_sites)}；输入原件没有修改"
        help_text = "保留并检查指出的精确 .tmp 文件，处理占用或权限后再重新运行"
    else:
        impact = "目标没有发布，固定临时文件已经清理；输入原件没有修改"
        help_text = "检查目标目录权限、占用和剩余空间后重试"
    details = {
        "object_name": str(target),
        "reason": reason,
        "impact": impact,
        "help_text": help_text,
    }
    cancellation = _cancellation_from(primary, cleanup)
    if cancellation is not None:
        raise ToolCancelledError(**details, cause=cancellation) from None
    raise ToolError(**details) from None


def _raise_file_published(
    primary: BaseException,
    *,
    target: Path,
    cleanup: BaseException | None = None,
    retained_sites: tuple[str, ...] = (),
    facts: tuple[str, ...] = (),
    cancellation: KeyboardInterrupt | None = None,
) -> NoReturn:
    reason_parts = [f"目标文件已经发布，但完成流程发生：{_failure_text(primary)}", *facts]
    if cleanup is not None:
        reason_parts.append(f"固定临时文件清理调用发生异常：{_failure_text(cleanup)}")
    if retained_sites:
        impact = f"目标 {target} 已经生效；临时文件保留于或需确认于 {' 与 '.join(retained_sites)}"
        help_text = "保留已发布目标，处理原因中指出的精确 .tmp 文件"
    else:
        impact = f"目标 {target} 已经生效；固定临时文件已经清理"
        help_text = "保留已发布目标并继续后续流程"
    raise OutputPublishedError(
        object_name=str(target),
        reason="；".join(reason_parts),
        impact=impact,
        help_text=help_text,
        cause=_cancellation_from(primary, cleanup, cancellation) or primary,
    ) from None


def _raise_unknown_file_publish(
    primary: BaseException,
    *,
    target: Path,
    temporary: Path,
    facts: tuple[str, ...],
    cancellation: KeyboardInterrupt | None = None,
) -> NoReturn:
    details = [f"文件发布调用发生：{_failure_text(primary)}", *facts]
    error = ToolError(
        object_name=str(target),
        reason="；".join(details),
        impact=f"无法确认目标文件是否已经发布；保留 {target} 与 {temporary} 作为核对现场",
        help_text="停止重试并核对指出的目标与固定 .tmp 文件内容后再处理",
    )
    cancelled = _cancellation_from(primary, cancellation)
    if cancelled is not None:
        raise ToolCancelledError(
            object_name=error.object_name,
            reason=error.reason,
            impact=error.impact,
            help_text=error.help_text,
            cause=cancelled,
        ) from None
    raise error from None


def _atomic_write_bytes(
    path: Path,
    body: bytes,
    *,
    replace: bool,
    temporary_suffix: str,
) -> None:
    target = _output_target(path, "输出文件")
    target.parent.mkdir(parents=True, exist_ok=True)
    _output_target(target, "输出文件")
    if (
        not temporary_suffix.startswith(".")
        or temporary_suffix in {".", ".."}
        or "/" in temporary_suffix
        or "\\" in temporary_suffix
        or ":" in temporary_suffix
        or any(ord(character) < 32 or ord(character) == 127 for character in temporary_suffix)
    ):
        fail(
            "固定临时文件后缀",
            "固定临时文件后缀不是同目录内的安全文件名片段",
            "使用以点开头且不含路径分隔符、盘符或控制字符的后缀",
        )
    temporary = target.with_name(f".{target.name}{temporary_suffix}")
    _output_target(temporary, "固定临时文件")
    if target.exists() and not replace:
        fail(str(target), "目标已存在", "换一个输出路径，或在确认可替换后传入 --replace")
    if temporary.exists():
        fail(str(temporary), "存在上次未清理的固定临时文件", "检查并处理该 .tmp 文件后重试")
    candidate_identity: _FileIdentity | None = None
    try:
        with temporary.open("xb") as handle:
            metadata = os.fstat(handle.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_ino == 0:
                raise OSError("固定临时文件没有可验证的普通文件身份")
            candidate_identity = (metadata.st_dev, metadata.st_ino)
            handle.write(body)
            handle.flush()
            os.fsync(handle.fileno())
    except FileExistsError:
        fail(str(temporary), "固定临时文件已由其他进程建立", "检查并处理该精确 .tmp 文件后重试")
    except BaseException as primary:  # noqa: BLE001 - 所有失败都必须清理已经建立的临时文件。
        cleanup, retained_sites = _candidate_file_cleanup(temporary, candidate_identity)
        _raise_file_failure(
            primary,
            cleanup,
            target=target,
            retained_sites=retained_sites,
        )
    try:
        _output_target(target, "输出文件")
    except BaseException as primary:  # noqa: BLE001 - 发布前元数据失败也必须清理候选文件。
        cleanup, retained_sites = _candidate_file_cleanup(temporary, candidate_identity)
        _raise_file_failure(
            primary,
            cleanup,
            target=target,
            retained_sites=retained_sites,
        )
    try:
        if replace:
            os.replace(temporary, target)
        else:
            os.link(temporary, target)
    except BaseException as primary:  # noqa: BLE001 - 发布失败必须保留主失败与清理结果。
        assert candidate_identity is not None
        state, facts, cancellation = _file_publish_state(temporary, target, candidate_identity)
        if state == "published":
            cleanup, retained_sites = _candidate_file_cleanup(temporary, candidate_identity)
            _raise_file_published(
                primary,
                target=target,
                cleanup=cleanup,
                retained_sites=retained_sites,
                facts=facts,
                cancellation=cancellation,
            )
        if state == "unknown":
            _raise_unknown_file_publish(
                primary,
                target=target,
                temporary=temporary,
                facts=facts,
                cancellation=cancellation,
            )
        cleanup, retained_sites = _candidate_file_cleanup(temporary, candidate_identity)
        if isinstance(primary, FileExistsError) and cleanup is None:
            fail(str(target), "目标在写入期间已由其他进程建立", "保留该文件并重新选择输出路径")
        _raise_file_failure(
            primary,
            cleanup,
            target=target,
            retained_sites=retained_sites,
            facts=facts,
        )
    assert candidate_identity is not None
    state, facts, cancellation = _file_publish_state(temporary, target, candidate_identity)
    if state != "published":
        _raise_unknown_file_publish(
            OSError("文件发布返回成功，但候选文件身份未到达目标路径"),
            target=target,
            temporary=temporary,
            facts=facts,
            cancellation=cancellation,
        )
    cleanup, retained_sites = _candidate_file_cleanup(temporary, candidate_identity)
    if cleanup is not None or retained_sites or facts:
        _raise_file_published(
            cleanup or OSError("文件发布后固定临时路径出现其他文件"),
            target=target,
            retained_sites=retained_sites,
            facts=facts,
            cancellation=cancellation,
        )


def atomic_write_bytes(
    path: Path,
    body: bytes,
    *,
    replace: bool,
    temporary_suffix: str = ".tmp",
) -> None:
    """用可核对身份的同目录候选文件原子发布原始字节。"""

    _atomic_write_bytes(
        path,
        body,
        replace=replace,
        temporary_suffix=temporary_suffix,
    )


def atomic_write_text(path: Path, text: str, *, replace: bool) -> None:
    """以无 BOM 的精确 UTF-8 字节发布文本，不做平台换行转换。"""

    atomic_write_bytes(path, text.encode("utf-8"), replace=replace)


def write_json(path: Path, value: JsonValue, *, replace: bool) -> None:
    try:
        text = json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True, allow_nan=False) + "\n"
    except ValueError:
        fail(str(path), "输出 JSON 包含非有限数字", "把数值改为有限 JSON number 后重新生成输出")
    atomic_write_text(
        path,
        text,
        replace=replace,
    )


_DirectoryIdentity = tuple[int, int]


class _ForeignDirectoryTypeError(OSError):
    """路径元数据可读取，但对象不是普通目录。"""


def _ordinary_directory_identity(path: Path) -> _DirectoryIdentity:
    metadata = path.lstat()
    file_attributes = getattr(metadata, "st_file_attributes", 0)
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or bool(file_attributes & 0x400):
        raise _ForeignDirectoryTypeError(f"{path} 不是普通目录")
    if metadata.st_ino == 0:
        raise OSError(f"{path} 所在文件系统没有提供稳定目录身份")
    return metadata.st_dev, metadata.st_ino


def _directory_identity_at(path: Path) -> tuple[_DirectoryIdentity | None, BaseException | None]:
    try:
        return _ordinary_directory_identity(path), None
    except FileNotFoundError:
        return None, None
    except BaseException as error:  # noqa: BLE001 - 发布终态必须保留无法确认的文件系统事实。
        return None, error


def _retained_directory_sites(
    *paths: Path,
) -> tuple[tuple[str, ...], KeyboardInterrupt | None]:
    sites: list[str] = []
    cancellation: KeyboardInterrupt | None = None
    for path in paths:
        identity, error = _directory_identity_at(path)
        if error is not None:
            sites.append(f"{path}（状态无法确认：{_failure_text(error)}）")
            cancellation = cancellation or _cancellation_from(error)
        elif identity is not None:
            sites.append(str(path))
    return tuple(sites), cancellation


def _restore_claimed_directory(
    claimed: Path,
    original: Path,
    claimed_identity: _DirectoryIdentity,
) -> tuple[Path, BaseException | None]:
    original_identity, original_error = _directory_identity_at(original)
    if original_error is not None or original_identity is not None:
        return claimed, original_error
    try:
        os.rename(claimed, original)
    except BaseException as error:  # noqa: BLE001 - 恢复操作的实际结果必须另行核对。
        restored_identity, inspection_error = _directory_identity_at(original)
        if restored_identity == claimed_identity:
            return original, error
        return claimed, inspection_error or error
    restored_identity, inspection_error = _directory_identity_at(original)
    if restored_identity == claimed_identity:
        return original, None
    return claimed, inspection_error or OSError("恢复后的目录身份与被认领对象不一致")


def remove_owned_directory(
    path: Path,
    expected_identity: _DirectoryIdentity,
    cleanup_path: Path,
) -> BaseException | None:
    """原子认领已知目录后删除，不按可能被替换的原路径递归。"""

    cleanup_identity, cleanup_error = _directory_identity_at(cleanup_path)
    if cleanup_error is not None:
        if isinstance(cleanup_error, KeyboardInterrupt):
            detail = str(cleanup_error).strip()
            suffix = f"（{sanitize_line(detail)}）" if detail else ""
            return KeyboardInterrupt(f"固定清理路径状态确认被取消：{cleanup_path}{suffix}")
        return OSError(f"固定清理路径 {cleanup_path} 状态无法确认：{_failure_text(cleanup_error)}")
    if cleanup_identity is not None:
        return OSError(f"固定清理路径已经存在，已保留现场：{path}；{cleanup_path}")

    source_identity, source_error = _directory_identity_at(path)
    if source_error is not None:
        return source_error
    if source_identity is None:
        return None
    if source_identity != expected_identity:
        return OSError(f"待清理目录的身份已经变化，已保留于 {path}")

    try:
        os.rename(path, cleanup_path)
    except BaseException as error:  # noqa: BLE001 - 认领返回错误时仍需要核对实际位置。
        state, facts, cancellation = _directory_move_state(path, cleanup_path, expected_identity)
        if state == "moved":
            return cancellation or error
        if state == "not_moved":
            return cancellation or error
        detail = f"；{'；'.join(facts)}" if facts else ""
        cancelled = _cancellation_from(error, cancellation)
        if cancelled is not None:
            return KeyboardInterrupt(
                f"取消发生后无法确认待清理目录的位置；需确认 {path} 与 {cleanup_path}{detail}"
            )
        return OSError(f"原子认领待清理目录失败（{type(error).__name__}）{detail}")

    claimed_identity, identity_error = _directory_identity_at(cleanup_path)
    if identity_error is not None or claimed_identity != expected_identity:
        identity_for_restore = claimed_identity or expected_identity
        residual, restore_error = _restore_claimed_directory(
            cleanup_path,
            path,
            identity_for_restore,
        )
        suffix = f"；恢复时发生 {type(restore_error).__name__}" if restore_error else ""
        reason = (
            f"无法确认被认领目录的身份（{type(identity_error).__name__}）"
            if identity_error is not None
            else "被认领目录的身份已经变化"
        )
        if isinstance(identity_error, KeyboardInterrupt):
            return identity_error
        if isinstance(restore_error, KeyboardInterrupt):
            return restore_error
        return OSError(f"{reason}，已保留于 {residual}{suffix}")

    try:
        shutil.rmtree(cleanup_path)
    except BaseException as error:  # noqa: BLE001 - 清理结果必须与主失败分开保存。
        remaining_identity, inspection_error = _directory_identity_at(cleanup_path)
        if inspection_error is not None:
            if isinstance(inspection_error, KeyboardInterrupt):
                detail = str(inspection_error).strip()
                suffix = f"（{sanitize_line(detail)}）" if detail else ""
                return KeyboardInterrupt(
                    f"清理调用失败后，固定清理路径状态确认被取消：{cleanup_path}{suffix}"
                )
            if isinstance(error, KeyboardInterrupt):
                return KeyboardInterrupt(
                    f"清理固定路径时命令被取消，且后验状态无法确认：{cleanup_path}"
                    f"（{_failure_text(inspection_error)}）"
                )
            return OSError(
                f"清理目录时发生 {_failure_text(error)}；固定清理路径 {cleanup_path} "
                f"的后验状态无法确认：{_failure_text(inspection_error)}"
            )
        if remaining_identity is None and inspection_error is None:
            return error
        if remaining_identity != expected_identity:
            if isinstance(error, KeyboardInterrupt):
                return KeyboardInterrupt(
                    f"清理固定路径时命令被取消，且清理边界的目录身份已经变化：{cleanup_path}"
                )
            return OSError(f"清理边界的目录身份已经变化，已保留于 {cleanup_path}")
        residual, restore_error = _restore_claimed_directory(
            cleanup_path,
            path,
            expected_identity,
        )
        suffix = f"；恢复时发生 {type(restore_error).__name__}" if restore_error else ""
        if isinstance(error, KeyboardInterrupt):
            return error
        if isinstance(restore_error, KeyboardInterrupt):
            return restore_error
        if restore_error is None:
            return error
        return OSError(f"清理目录时发生 {type(error).__name__}，已保留于 {residual}{suffix}")
    return None


def _directory_move_state(
    source: Path,
    destination: Path,
    expected_identity: _DirectoryIdentity,
) -> tuple[
    Literal["moved", "not_moved", "unknown"],
    tuple[str, ...],
    KeyboardInterrupt | None,
]:
    source_identity, source_error = _directory_identity_at(source)
    destination_identity, destination_error = _directory_identity_at(destination)
    facts: list[str] = []
    if source_error is not None:
        label = "不是普通目录" if isinstance(source_error, _ForeignDirectoryTypeError) else "状态无法读取"
        facts.append(f"源目录 {source} {label}：{_failure_text(source_error)}")
    if destination_error is not None:
        label = (
            "不是普通目录" if isinstance(destination_error, _ForeignDirectoryTypeError) else "状态无法读取"
        )
        facts.append(f"目标目录 {destination} {label}：{_failure_text(destination_error)}")
    cancellation = _cancellation_from(source_error, destination_error)
    if destination_error is None and destination_identity == expected_identity:
        if source_error is None and source_identity == expected_identity:
            facts.append("源目录与目标目录同时具有预期身份")
            return "unknown", tuple(facts), cancellation
        if source_error is None and source_identity is not None:
            facts.append(f"源固定目录已由其他对象占用，保留于 {source}")
        return "moved", tuple(facts), cancellation
    if source_identity == expected_identity and (
        destination_error is None or isinstance(destination_error, _ForeignDirectoryTypeError)
    ):
        return "not_moved", tuple(facts), cancellation
    if facts:
        return "unknown", tuple(facts), cancellation
    return "unknown", (), cancellation


def _raise_unknown_directory_move(
    primary: BaseException,
    *,
    source: Path,
    destination: Path,
    facts: tuple[str, ...],
    cancellation: KeyboardInterrupt | None = None,
) -> NoReturn:
    details = [f"目录交换返回错误：{_failure_text(primary)}", *facts]
    error = ToolError(
        object_name=str(destination.parent),
        reason="；".join(details),
        impact=f"无法确认目录交换结果；{source} 与 {destination} 保持为恢复现场",
        help_text="停止重试并保留指出的固定路径，确认两个目录的内容和身份后再处理",
    )
    cancelled = _cancellation_from(primary, cancellation)
    if cancelled is not None:
        raise ToolCancelledError(
            object_name=error.object_name,
            reason=error.reason,
            impact=error.impact,
            help_text=error.help_text,
            cause=cancelled,
        ) from None
    raise error from None


def _restore_previous_directory(
    previous: Path,
    target: Path,
    expected_identity: _DirectoryIdentity,
    *,
    publish_error: BaseException,
) -> BaseException | None:
    try:
        os.replace(previous, target)
    except BaseException as restore_error:  # noqa: BLE001 - 恢复移动也必须确认实际终态。
        state, facts, cancellation = _directory_move_state(previous, target, expected_identity)
        if state == "moved":
            return cancellation or restore_error
        if state == "unknown":
            _raise_unknown_directory_move(
                restore_error,
                source=previous,
                destination=target,
                facts=(f"新目录发布失败：{_failure_text(publish_error)}", *facts),
                cancellation=cancellation,
            )
        details = {
            "object_name": str(target.parent),
            "reason": (
                f"新目录发布失败：{_failure_text(publish_error)}；"
                f"旧目录恢复失败：{_failure_text(restore_error)}" + (f"；{'；'.join(facts)}" if facts else "")
            ),
            "impact": f"旧目录仍位于 {previous}；完整临时目录保持原样；目标尚未恢复",
            "help_text": "停止重试并保留 .tmp/.previous，处理权限或占用后恢复旧目录",
        }
        cancelled = _cancellation_from(publish_error, restore_error, cancellation)
        if cancelled is not None:
            raise ToolCancelledError(**details, cause=cancelled) from None
        raise ToolError(**details) from None
    state, facts, cancellation = _directory_move_state(previous, target, expected_identity)
    if state == "moved":
        if cancellation is not None:
            return cancellation
        if facts:
            return OSError(f"旧目录已经恢复；{'；'.join(facts)}")
        return None
    _raise_unknown_directory_move(
        OSError("旧目录恢复返回成功，但目录身份未到达目标位置"),
        source=previous,
        destination=target,
        facts=(f"新目录发布失败：{_failure_text(publish_error)}", *facts),
        cancellation=cancellation,
    )


def _raise_published_directory_error(
    primary: BaseException,
    *,
    target: Path,
    previous: Path,
    previous_identity: _DirectoryIdentity | None,
    previous_cleanup: Path,
    move_facts: tuple[str, ...] = (),
    cancellation: KeyboardInterrupt | None = None,
) -> NoReturn:
    cleanup = (
        remove_owned_directory(previous, previous_identity, previous_cleanup)
        if previous_identity is not None
        else None
    )
    details = f"目录已经发布，但发布调用返回前发生：{_failure_text(primary)}"
    impact = f"新目录 {target} 已经生效"
    help_text = "保留已经发布的目标；确认内容后继续后续流程"
    if move_facts:
        details += f"；{'；'.join(move_facts)}"
        impact += "；源固定路径的现场保持原样"
        help_text = "保留已经发布的目标和原因中指出的源固定路径，确认内容后继续后续流程"
    previous_after, previous_error = _directory_identity_at(previous)
    cleanup_after, cleanup_error = _directory_identity_at(previous_cleanup)
    for path, inspection_error in (
        (previous, previous_error),
        (previous_cleanup, cleanup_error),
    ):
        if inspection_error is not None:
            details += f"；固定路径 {path} 状态无法确认：{_failure_text(inspection_error)}"
    if cleanup is not None:
        if (
            previous_after is None
            and previous_error is None
            and cleanup_after is None
            and cleanup_error is None
        ):
            details += f"；旧目录清理完成前发生：{_failure_text(cleanup)}"
        else:
            details += f"；旧目录清理失败：{_failure_text(cleanup)}"
            retained = [
                str(path)
                for path, identity, inspection_error in (
                    (previous, previous_after, previous_error),
                    (previous_cleanup, cleanup_after, cleanup_error),
                )
                if identity is not None or inspection_error is not None
            ]
            impact += f"；旧目录仍位于或需确认于 {' 与 '.join(retained)}"
            help_text = "保留已经发布的目标和原因中指出的固定路径，处理 .previous/.cleanup 目录"
    else:
        retained = [
            str(path)
            for path, identity, inspection_error in (
                (previous, previous_after, previous_error),
                (previous_cleanup, cleanup_after, cleanup_error),
            )
            if identity is not None or inspection_error is not None
        ]
        if retained:
            impact += f"；旧目录仍位于或需确认于 {' 与 '.join(retained)}"
            help_text = "保留已经发布的目标和原因中指出的固定路径，处理 .previous/.cleanup 目录"
    cause: BaseException = (
        _cancellation_from(primary, cancellation, cleanup, previous_error, cleanup_error) or primary
    )
    raise OutputPublishedError(
        object_name=str(target),
        reason=details,
        impact=impact,
        help_text=help_text,
        cause=cause,
    ) from None


def _raise_directory_cleanup_failure(
    primary: BaseException,
    cleanup: BaseException,
    *,
    target: Path,
    stage: Path,
    stage_cleanup: Path,
    restored: bool,
    related_error: BaseException | None = None,
    facts: tuple[str, ...] = (),
) -> NoReturn:
    restored_text = "原目标仍位于目标路径" if restored else "目标没有由本工具发布"
    retained_sites, probe_cancellation = _retained_directory_sites(stage, stage_cleanup)
    if retained_sites:
        cleanup_impact = f"临时目录保留于或需确认于 {' 与 '.join(retained_sites)}"
        help_text = "保留并检查指出的精确 .tmp/.cleanup 目录，处理占用或权限后再重新运行"
    else:
        cleanup_impact = "后验确认固定临时目录已经清理"
        help_text = "检查目标目录权限、占用和剩余空间后重试"
    reason_parts = [f"目录写入或发布失败：{_failure_text(primary)}", *facts]
    if related_error is not None:
        reason_parts.append(f"恢复调用发生：{_failure_text(related_error)}")
    reason_parts.append(f"临时目录清理也失败：{_failure_text(cleanup)}")
    details = {
        "object_name": str(target),
        "reason": "；".join(reason_parts),
        "impact": f"{restored_text}；{cleanup_impact}；输入原件没有修改",
        "help_text": help_text,
    }
    cancellation = _cancellation_from(primary, related_error, cleanup, probe_cancellation)
    if cancellation is not None:
        raise ToolCancelledError(
            **details,
            cause=cancellation,
        ) from None
    raise ToolError(
        **details,
    ) from None


def _raise_known_directory_failure(
    primary: BaseException,
    *,
    target: Path,
    restored: bool,
    related_error: BaseException | None = None,
    facts: tuple[str, ...] = (),
) -> NoReturn:
    reason_parts = [f"目录写入或发布失败：{_failure_text(primary)}", *facts]
    if related_error is not None:
        reason_parts.append(f"恢复调用发生：{_failure_text(related_error)}")
    details = {
        "object_name": str(target),
        "reason": "；".join(reason_parts),
        "impact": (
            ("原目标仍位于目标路径" if restored else "目标没有由本工具发布")
            + "；固定临时目录已经清理；输入原件没有修改"
        ),
        "help_text": "检查目标目录权限、占用和剩余空间后重试",
    }
    cancellation = _cancellation_from(primary, related_error)
    if cancellation is not None:
        raise ToolCancelledError(**details, cause=cancellation) from None
    raise ToolError(**details) from None


def _raise_stage_setup_failure(
    primary: BaseException,
    *,
    target: Path,
    stage: Path,
    stage_cleanup: Path,
    cleaned: bool = False,
) -> NoReturn:
    retained_sites, probe_cancellation = _retained_directory_sites(stage, stage_cleanup)
    if retained_sites:
        impact = f"目标没有发布；固定临时目录保留于或需确认于 {' 与 '.join(retained_sites)}；输入原件没有修改"
    elif cleaned:
        impact = "目标没有发布；固定临时目录已经清理；输入原件没有修改"
    else:
        impact = "目标没有发布；后验确认固定临时目录不存在；输入原件没有修改"
    help_text = (
        "保留并检查指出的精确 .tmp/.cleanup 目录，处理占用或权限后再重新运行"
        if retained_sites
        else "检查目标目录权限、占用和剩余空间后重试"
    )
    details = {
        "object_name": str(target),
        "reason": f"固定临时目录建立或身份确认失败：{_failure_text(primary)}",
        "impact": impact,
        "help_text": help_text,
    }
    cancellation = _cancellation_from(primary, probe_cancellation)
    if cancellation is not None:
        raise ToolCancelledError(**details, cause=cancellation) from None
    raise ToolError(**details) from None


def _create_directory_stage(target: Path, stage: Path, stage_cleanup: Path) -> _DirectoryIdentity:
    try:
        stage.mkdir()
    except FileExistsError:
        fail(
            str(stage),
            "固定临时目录已由其他进程建立",
            "检查并处理该精确 .tmp 目录后重试",
        )
    except BaseException as primary:  # noqa: BLE001 - mkdir 返回异常后需核对实际固定现场。
        _raise_stage_setup_failure(
            primary,
            target=target,
            stage=stage,
            stage_cleanup=stage_cleanup,
        )
    try:
        return _ordinary_directory_identity(stage)
    except BaseException as primary:  # noqa: BLE001 - 身份探测取消也必须清理已建立的候选目录。
        recovered_identity, identity_error = _directory_identity_at(stage)
        if recovered_identity is None:
            _raise_stage_setup_failure(
                identity_error or primary,
                target=target,
                stage=stage,
                stage_cleanup=stage_cleanup,
            )
        cleanup = remove_owned_directory(stage, recovered_identity, stage_cleanup)
        if cleanup is not None:
            _raise_directory_cleanup_failure(
                primary,
                cleanup,
                target=target,
                stage=stage,
                stage_cleanup=stage_cleanup,
                restored=False,
            )
        _raise_stage_setup_failure(
            primary,
            target=target,
            stage=stage,
            stage_cleanup=stage_cleanup,
            cleaned=True,
        )


def _cleanup_directory_stage_after_failure(
    primary: BaseException,
    *,
    target: Path,
    stage: Path,
    stage_identity: _DirectoryIdentity,
    stage_cleanup: Path,
    restored: bool,
    related_error: BaseException | None = None,
    facts: tuple[str, ...] = (),
) -> NoReturn:
    cleanup = remove_owned_directory(stage, stage_identity, stage_cleanup)
    if cleanup is not None:
        _raise_directory_cleanup_failure(
            primary,
            cleanup,
            target=target,
            stage=stage,
            stage_cleanup=stage_cleanup,
            restored=restored,
            related_error=related_error,
            facts=facts,
        )
    _raise_known_directory_failure(
        primary,
        target=target,
        restored=restored,
        related_error=related_error,
        facts=facts,
    )


def atomic_write_directory(
    target_path: Path, files: Mapping[str, str | bytes | Path], *, replace: bool
) -> None:
    """原子发布由 UTF-8 文本、原始字节或普通源文件组成的目录。"""

    target = _output_target(target_path, "输出目录")
    target.parent.mkdir(parents=True, exist_ok=True)
    _output_target(target, "输出目录")
    stage = target.with_name(f".{target.name}.tmp")
    previous = target.with_name(f".{target.name}.previous")
    stage_cleanup = target.with_name(f".{target.name}.tmp.cleanup")
    previous_cleanup = target.with_name(f".{target.name}.previous.cleanup")
    _output_target(stage, "固定临时目录")
    _output_target(previous, "固定恢复目录")
    _output_target(stage_cleanup, "固定临时清理目录")
    _output_target(previous_cleanup, "固定恢复清理目录")
    if target.exists() and not target.is_dir():
        fail(str(target), "目标已存在且不是普通目录", "为目录输出选择不存在的路径或已有普通目录")
    if target.exists() and not replace:
        fail(str(target), "目标目录已存在", "换一个输出目录，或在确认可替换后传入 --replace")
    retained = [path for path in (stage, previous, stage_cleanup, previous_cleanup) if path.exists()]
    if retained:
        fail(
            str(target.parent),
            f"存在上次未清理的固定现场：{'; '.join(str(path) for path in retained)}",
            "检查并处理指出的 .tmp/.previous/.cleanup 目录后重试",
        )
    stage_identity = _create_directory_stage(target, stage, stage_cleanup)
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
    except BaseException as primary:  # noqa: BLE001 - 所有失败都必须清理已建立的候选目录。
        _cleanup_directory_stage_after_failure(
            primary,
            target=target,
            stage=stage,
            stage_identity=stage_identity,
            stage_cleanup=stage_cleanup,
            restored=False,
        )

    moved_previous = False
    previous_identity: _DirectoryIdentity | None = None
    try:
        _output_target(target, "输出目录")
        _output_target(previous, "固定恢复目录")
        target_exists = target.exists()
        if target_exists and not replace:
            fail(str(target), "目标在写入期间已由其他进程建立", "保留该目录并重新选择输出路径")
        if target_exists and not target.is_dir():
            fail(str(target), "目标在写入期间变成非目录", "保留该文件并重新选择目录输出路径")
        if target_exists:
            previous_identity = _ordinary_directory_identity(target)
    except BaseException as primary:  # noqa: BLE001 - 发布前元数据失败也必须清理候选目录。
        _cleanup_directory_stage_after_failure(
            primary,
            target=target,
            stage=stage,
            stage_identity=stage_identity,
            stage_cleanup=stage_cleanup,
            restored=False,
        )
    if target_exists:
        assert previous_identity is not None
        try:
            os.replace(target, previous)
        except BaseException as primary:  # noqa: BLE001 - 移动取消也必须核对并恢复实际终态。
            state, facts, cancellation = _directory_move_state(target, previous, previous_identity)
            if state == "unknown":
                _raise_unknown_directory_move(
                    primary,
                    source=target,
                    destination=previous,
                    facts=facts,
                    cancellation=cancellation,
                )
            restore_error = None
            if state == "moved":
                restore_error = _restore_previous_directory(
                    previous,
                    target,
                    previous_identity,
                    publish_error=primary,
                )
            _cleanup_directory_stage_after_failure(
                primary,
                target=target,
                stage=stage,
                stage_identity=stage_identity,
                stage_cleanup=stage_cleanup,
                restored=True,
                related_error=restore_error,
                facts=facts,
            )
        state, facts, cancellation = _directory_move_state(target, previous, previous_identity)
        if state != "moved" or facts:
            _raise_unknown_directory_move(
                OSError("旧目标移动返回成功，但目录身份未到达恢复位置"),
                source=target,
                destination=previous,
                facts=facts,
                cancellation=cancellation,
            )
        moved_previous = True
    try:
        if replace:
            os.replace(stage, target)
        else:
            os.rename(stage, target)
    except BaseException as primary:  # noqa: BLE001 - 发布取消也必须核对并恢复实际终态。
        state, facts, cancellation = _directory_move_state(stage, target, stage_identity)
        if state == "moved":
            _raise_published_directory_error(
                primary,
                target=target,
                previous=previous,
                previous_identity=previous_identity if moved_previous else None,
                previous_cleanup=previous_cleanup,
                move_facts=facts,
                cancellation=cancellation,
            )
        if state == "unknown":
            _raise_unknown_directory_move(
                primary,
                source=stage,
                destination=target,
                facts=facts,
                cancellation=cancellation,
            )
        restore_error = None
        if moved_previous:
            if previous_identity is None:
                raise AssertionError("移动旧目录后缺少目录身份") from None
            restore_error = _restore_previous_directory(
                previous,
                target,
                previous_identity,
                publish_error=primary,
            )
        _cleanup_directory_stage_after_failure(
            primary,
            target=target,
            stage=stage,
            stage_identity=stage_identity,
            stage_cleanup=stage_cleanup,
            restored=moved_previous,
            related_error=restore_error,
            facts=facts,
        )
    state, facts, cancellation = _directory_move_state(stage, target, stage_identity)
    if state == "moved" and facts:
        _raise_published_directory_error(
            OSError("新目录发布后源固定路径出现其他对象"),
            target=target,
            previous=previous,
            previous_identity=previous_identity if moved_previous else None,
            previous_cleanup=previous_cleanup,
            move_facts=facts,
            cancellation=cancellation,
        )
    if state != "moved":
        _raise_unknown_directory_move(
            OSError("新目录发布返回成功，但候选目录身份未到达目标位置"),
            source=stage,
            destination=target,
            facts=facts,
            cancellation=cancellation,
        )
    if moved_previous:
        if previous_identity is None:
            raise AssertionError("移动旧目录后缺少目录身份") from None
        cleanup = remove_owned_directory(previous, previous_identity, previous_cleanup)
        if cleanup is not None:
            _raise_published_directory_error(
                cleanup,
                target=target,
                previous=previous,
                previous_identity=None,
                previous_cleanup=previous_cleanup,
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
