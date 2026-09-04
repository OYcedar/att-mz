"""RPG Maker 字体替换的前后字节快照、apply 回滚与 drift-safe restore。"""

from __future__ import annotations

import hashlib
import json
import os
import stat
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import NoReturn, Protocol, cast

from att_skill_tools import (
    OutputPublishedError,
    ToolCancelledError,
    ToolError,
    atomic_write_bytes,
    fail,
    remove_owned_directory,
)

_STATE_STATUSES = frozenset({"prepared", "applied", "rolled_back", "recovery_required", "restored"})


@dataclass(frozen=True, slots=True)
class ByteMutation:
    relative_path: str
    original: bytes | None
    replacement: bytes


@dataclass(frozen=True, slots=True)
class FontStateBinding:
    """apply 发布后绑定的恢复目录身份与本次计划快照。"""

    path: Path
    identity: tuple[int, int]
    plan_files: tuple[tuple[str, bytes], ...]


@dataclass(frozen=True, slots=True)
class _RestoreStateBinding:
    """restore 期间绑定的 state 目录与 manifest。"""

    path: Path
    identity: tuple[int, int]
    manifest_identity: tuple[int, int, int, int]
    manifest_sha256: str


@dataclass(frozen=True, slots=True)
class _RestoreEntry:
    """不携带快照正文的 restore 清单项。"""

    relative_path: str
    before_file: str | None
    before_sha256: str | None
    after_file: str
    after_sha256: str


@dataclass(frozen=True, slots=True)
class FontGameLock:
    """同一游戏字体写操作持有的固定目录锁。"""

    path: Path
    cleanup_path: Path
    identity: tuple[int, int]


@dataclass(frozen=True, slots=True)
class FontGameLockRelease:
    """任务锁释放调用及两个固定路径的后验事实。"""

    errors: tuple[BaseException, ...]
    retained_sites: tuple[Path, ...]
    uncertain_sites: tuple[Path, ...]


@dataclass(slots=True)
class FontStateIntegrityError(ToolError):
    """已绑定的字体恢复 state 被替换或修改。"""


class FontTransactionPlan(Protocol):
    @property
    def game_root(self) -> Path: ...

    @property
    def selected_font(self) -> Path: ...

    @property
    def selected_sha256(self) -> str: ...

    @property
    def mutations(self) -> tuple[ByteMutation, ...]: ...


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _status_body(status: str) -> bytes:
    return (
        json.dumps(
            {"status": status},
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")


def font_state_files(plan: FontTransactionPlan) -> Mapping[str, str | bytes | Path]:
    """返回应原子发布到 state 目录的完整事务快照。"""

    entries: list[dict[str, object]] = []
    files: dict[str, str | bytes | Path] = {}
    for index, mutation in enumerate(plan.mutations, start=1):
        before_name: str | None = None
        if mutation.original is not None:
            before_name = f"before/{index:06d}.bin"
            files[before_name] = mutation.original
        after_name = f"after/{index:06d}.bin"
        files[after_name] = mutation.replacement
        entries.append(
            {
                "path": mutation.relative_path,
                "before_file": before_name,
                "before_sha256": sha256_bytes(mutation.original) if mutation.original is not None else None,
                "after_file": after_name,
                "after_sha256": sha256_bytes(mutation.replacement),
                "created": mutation.original is None,
            }
        )
    manifest = {
        "game_root": str(plan.game_root),
        "selected_font_name": plan.selected_font.name,
        "selected_font_sha256": plan.selected_sha256,
        "entries": entries,
    }
    files["manifest.json"] = json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    files["status.json"] = _status_body("prepared")
    return files


def _state_integrity_error(path: Path, reason: str, *, replaced: bool = False) -> NoReturn:
    impact = (
        "当前 state 路径不再是已绑定目录；原恢复 state 的残留位置无法确认"
        if replaced
        else "当前 state 仍是已绑定目录，但恢复内容已被修改，不能作为恢复依据"
    )
    raise FontStateIntegrityError(
        object_name=str(Path(os.path.abspath(path))),
        reason=reason,
        impact=impact,
        help_text="停止本次 apply，保留游戏目录和现有 state 路径并人工核对",
    )


def _directory_identity(path: Path) -> tuple[int, int]:
    metadata = path.lstat()
    file_attributes = getattr(metadata, "st_file_attributes", 0)
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or bool(file_attributes & 0x400):
        raise OSError("路径不是普通目录")
    if metadata.st_ino == 0:
        raise OSError("文件系统没有提供稳定目录身份")
    return metadata.st_dev, metadata.st_ino


def font_game_lock_paths(game_root: Path) -> tuple[Path, Path]:
    """返回同一自然游戏根共用的固定锁目录与清理目录。"""

    root = Path(os.path.abspath(game_root))
    try:
        lock = root.with_name(f".{root.name}.att-font.lock")
    except ValueError:
        fail(str(root), "游戏根不能直接使用文件系统根目录", "传入精确的 RPG Maker 游戏目录")
    return lock, lock.with_name(f"{lock.name}.cleanup")


def _path_present(path: Path) -> bool:
    try:
        path.lstat()
    except FileNotFoundError:
        return False
    except OSError as error:
        fail(
            str(path),
            f"字体任务锁路径状态无法读取（{type(error).__name__}）",
            "恢复该精确路径的读取权限后重试",
        )
    return True


def acquire_font_game_lock(game_root: Path) -> FontGameLock:
    """原子建立同一游戏唯一的固定字体任务目录锁。"""

    lock, cleanup = font_game_lock_paths(game_root)
    if _path_present(cleanup):
        fail(
            str(cleanup),
            "存在上次保留的字体任务锁清理现场",
            "确认对应字体任务已经结束，处理这个精确 .lock.cleanup 目录后重试",
        )
    try:
        lock.mkdir()
    except FileExistsError:
        fail(
            str(lock),
            "同一游戏已有字体 apply/restore 任务或上次保留的任务锁",
            "等待对应任务结束；确认没有任务运行后处理这个精确 .lock 目录并重试",
        )
    except KeyboardInterrupt as error:
        probe_error: BaseException | None = None
        try:
            retained = _path_present(lock)
        except BaseException as inspection_error:  # noqa: BLE001 - 原取消与锁现场都必须保留。
            retained = True
            probe_error = inspection_error
        reason = "建立字体任务锁时使用者取消了命令"
        if probe_error is not None:
            reason += f"；任务锁状态复核发生 {_font_failure_reason(probe_error)}"
        raise ToolCancelledError(
            object_name=str(lock),
            reason=reason,
            impact=(
                f"游戏、state 和结果尚未修改；任务锁需确认于 {lock}"
                if retained
                else "游戏、state 和结果尚未修改；任务锁没有建立"
            ),
            help_text=("确认没有字体任务运行后处理这个精确 .lock 目录" if retained else "可直接重新运行命令"),
            cause=error,
        ) from None
    except OSError as error:
        fail(
            str(lock),
            f"字体任务锁无法建立（{type(error).__name__}）",
            "检查游戏父目录权限后重试",
        )
    try:
        identity = _directory_identity(lock)
    except BaseException as error:  # noqa: BLE001 - 已建锁身份未知时必须保留精确现场。
        details = {
            "object_name": str(lock),
            "reason": f"字体任务锁已经建立，但目录身份无法确认（{type(error).__name__}）",
            "impact": f"游戏、state 和结果尚未修改；任务锁保留于 {lock}",
            "help_text": "确认没有字体任务运行后检查并处理这个精确 .lock 目录",
        }
        if isinstance(error, KeyboardInterrupt):
            raise ToolCancelledError(**details, cause=error) from None
        raise ToolError(**details) from None
    return FontGameLock(lock, cleanup, identity)


def release_font_game_lock(lock: FontGameLock) -> FontGameLockRelease:
    """按已绑定身份清理任务锁，并返回两个固定路径的后验事实。"""

    cleanup_error = remove_owned_directory(lock.path, lock.identity, lock.cleanup_path)
    errors: list[BaseException] = [] if cleanup_error is None else [cleanup_error]
    retained_sites: list[Path] = []
    uncertain_sites: list[Path] = []
    for path in (lock.path, lock.cleanup_path):
        try:
            path.lstat()
        except FileNotFoundError:
            continue
        except BaseException as error:  # noqa: BLE001 - 锁释放必须传播后验位置与取消事实。
            uncertain_sites.append(path)
            errors.append(error)
        else:
            retained_sites.append(path)
    if (retained_sites or uncertain_sites) and not errors:
        errors.append(OSError("字体任务锁释放返回后仍存在或无法确认固定锁路径"))
    return FontGameLockRelease(tuple(errors), tuple(retained_sites), tuple(uncertain_sites))


def _require_bound_identity(state: Path, binding: FontStateBinding) -> None:
    if Path(os.path.abspath(state)) != binding.path:
        _state_integrity_error(state, "字体 state 路径与发布后绑定的路径不一致", replaced=True)
    try:
        identity = _directory_identity(state)
    except OSError:
        _state_integrity_error(state, "字体 state 已被移走、替换或不再是普通目录", replaced=True)
    if identity != binding.identity:
        _state_integrity_error(state, "字体 state 目录身份与发布后绑定的目录不一致", replaced=True)


def _plan_state_files(plan: FontTransactionPlan) -> tuple[tuple[str, bytes], ...]:
    files: list[tuple[str, bytes]] = []
    for relative, value in font_state_files(plan).items():
        if relative == "status.json":
            continue
        if isinstance(value, Path):
            try:
                body = value.read_bytes()
            except OSError as error:
                _state_integrity_error(value, f"字体 plan 的 state 源文件无法读取（{type(error).__name__}）")
        else:
            body = value if isinstance(value, bytes) else value.encode("utf-8")
        files.append((relative, body))
    return tuple(sorted(files))


def _state_file(state: Path, relative_text: str) -> bytes:
    relative = PurePosixPath(relative_text)
    for parent in relative.parents:
        if parent == PurePosixPath("."):
            continue
        try:
            _directory_identity(state.joinpath(*parent.parts))
        except OSError:
            _state_integrity_error(state / relative_text, f"字体 state 的 {relative_text} 路径结构已变化")
    path = state.joinpath(*relative.parts)
    try:
        metadata = path.lstat()
        file_attributes = getattr(metadata, "st_file_attributes", 0)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or stat.S_ISLNK(metadata.st_mode)
            or bool(file_attributes & 0x400)
        ):
            raise OSError("路径不是普通文件")
        return path.read_bytes()
    except OSError:
        _state_integrity_error(path, f"字体 state 的 {relative_text} 缺失或不再是普通文件")


def _verify_font_state(
    plan: FontTransactionPlan,
    *,
    state: Path,
    binding: FontStateBinding,
    statuses: frozenset[str],
    marker: bytes | None,
    full: bool,
) -> None:
    _require_bound_identity(state, binding)
    if full:
        if _plan_state_files(plan) != binding.plan_files:
            _state_integrity_error(state, "本次字体 plan 在 state 发布后发生变化")
        for relative, expected in binding.plan_files:
            if _state_file(state, relative) != expected:
                _state_integrity_error(state / relative, f"字体 state 的 {relative} 与本次 plan 不一致")
    if _state_file(state, "status.json") not in {_status_body(value) for value in statuses}:
        expected = " 或 ".join(sorted(statuses))
        _state_integrity_error(state / "status.json", f"字体 state 状态不是预期的 {expected}")
    marker_path = state / "applied.json"
    if marker is None:
        if marker_path.exists() or marker_path.is_symlink():
            _state_integrity_error(marker_path, "字体 state 提前出现 applied 标记")
    elif _state_file(state, "applied.json") != marker:
        _state_integrity_error(marker_path, "字体 state 的 applied 标记与本次结果不一致")
    _require_bound_identity(state, binding)


def bind_font_state(plan: FontTransactionPlan, *, state: Path) -> FontStateBinding:
    """绑定刚发布的 canonical state；返回前确认 prepared 目录及全部快照。"""

    try:
        identity = _directory_identity(state)
    except OSError:
        _state_integrity_error(state, "刚发布的字体 state 缺失或不是普通目录", replaced=True)
    binding = FontStateBinding(Path(os.path.abspath(state)), identity, _plan_state_files(plan))
    _verify_font_state(
        plan,
        state=state,
        binding=binding,
        statuses=frozenset({"prepared"}),
        marker=None,
        full=True,
    )
    return binding


def _verify_font_state_binding(
    plan: FontTransactionPlan,
    *,
    state: Path,
    binding: FontStateBinding,
    expected_status: str,
    applied_marker: bytes | None,
) -> None:
    _verify_font_state(
        plan,
        state=state,
        binding=binding,
        statuses=frozenset({expected_status}),
        marker=applied_marker,
        full=True,
    )


def _atomic_write_bytes(target: Path, body: bytes, *, expect_missing: bool) -> None:
    atomic_write_bytes(
        target,
        body,
        replace=not expect_missing,
        temporary_suffix=".att-font.tmp",
    )


def _target(game_root: Path, relative_text: str) -> Path:
    relative = PurePosixPath(relative_text)
    if (
        not relative.parts
        or relative.is_absolute()
        or ".." in relative.parts
        or "\\" in relative_text
        or ":" in relative_text
    ):
        fail(relative_text, "字体事务路径超出游戏根", "不要编辑事务清单；重新 inspect/apply")
    natural_root = Path(os.path.abspath(game_root))
    target = natural_root.joinpath(*relative.parts)
    try:
        target.relative_to(natural_root)
    except ValueError:
        fail(relative_text, "字体事务路径解析后超出游戏根", "不要编辑事务清单；重新 inspect/apply")
    current = natural_root
    for index, part in enumerate((None, *relative.parts)):
        if part is not None:
            current = current / part
        try:
            metadata = current.lstat()
        except FileNotFoundError:
            if index == 0:
                fail(str(natural_root), "字体事务游戏根不存在", "传入当前可读取的 RPG Maker 游戏目录")
            break
        except OSError as error:
            fail(
                str(current),
                f"字体事务路径元数据无法读取（{type(error).__name__}）",
                "恢复该自然路径的读取权限后重新 inspect/apply",
            )
        file_attributes = getattr(metadata, "st_file_attributes", 0)
        if stat.S_ISLNK(metadata.st_mode) or bool(file_attributes & 0x400):
            fail(
                str(current),
                "字体事务路径包含符号链接或 Windows reparse point",
                "把字体目标恢复为游戏根内的普通目录和普通文件后重新 inspect/apply",
            )
        if current != target and not stat.S_ISDIR(metadata.st_mode):
            fail(
                str(current),
                "字体事务目标的父路径不是普通目录",
                "把该路径恢复为普通目录后重新 inspect/apply",
            )
    return target


def _rollback_to_original(
    game_root: Path,
    attempted: list[ByteMutation],
) -> tuple[BaseException, ...]:
    """只回滚已尝试项；遇到未知并发字节时不覆盖，并继续检查其余项。"""

    failures: list[BaseException] = []
    for mutation in reversed(attempted):
        try:
            target = _target(game_root, mutation.relative_path)
            if mutation.original is None:
                if not target.exists():
                    continue
                if not target.is_file() or sha256_bytes(target.read_bytes()) != sha256_bytes(
                    mutation.replacement
                ):
                    raise OSError("已尝试的新文件出现未知并发字节")
                target.unlink()
                continue
            if not target.is_file():
                raise OSError("已尝试的原文件不再是普通文件")
            current_sha = sha256_bytes(target.read_bytes())
            original_sha = sha256_bytes(mutation.original)
            if current_sha == original_sha:
                continue
            if current_sha != sha256_bytes(mutation.replacement):
                raise OSError("已尝试的文件出现未知并发字节")
            _atomic_write_bytes(target, mutation.original, expect_missing=False)
        except BaseException as error:  # noqa: BLE001 - 每一项都必须尽力回滚。
            failures.append(error)
    return tuple(failures)


def _write_state_status(state: Path, status: str) -> None:
    if status not in _STATE_STATUSES:
        raise ValueError(f"未知字体事务状态：{status}")
    status_path = state / "status.json"
    if not status_path.is_file():
        fail(str(status_path), "字体 state 缺少机器状态", "恢复完整 state 后重试")
    _atomic_write_bytes(status_path, _status_body(status), expect_missing=False)


def _write_bound_state_status(
    plan: FontTransactionPlan,
    *,
    state: Path,
    binding: FontStateBinding,
    expected_statuses: frozenset[str],
    status: str,
) -> None:
    _verify_font_state(
        plan,
        state=state,
        binding=binding,
        statuses=expected_statuses,
        marker=None,
        full=True,
    )
    _write_state_status(state, status)
    _verify_font_state(
        plan,
        state=state,
        binding=binding,
        statuses=frozenset({status}),
        marker=None,
        full=True,
    )


def _read_state_status(state: Path) -> str:
    status_path = state / "status.json"
    try:
        value = cast(object, json.loads(status_path.read_text(encoding="utf-8")))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(str(status_path), f"字体 state 状态无法读取（{type(error).__name__}）", "恢复完整 state 后重试")
    if not isinstance(value, dict):
        fail(str(status_path), "字体 state 状态结构无效", "不要编辑 state")
    status = cast(dict[object, object], value).get("status")
    if not isinstance(status, str) or status not in _STATE_STATUSES:
        fail(str(status_path), "字体 state 状态值无效", "不要编辑 state")
    return status


def _cancellation_cause(error: BaseException | None) -> KeyboardInterrupt | None:
    if isinstance(error, KeyboardInterrupt):
        return error
    cause = getattr(error, "cause", None)
    return cause if isinstance(cause, KeyboardInterrupt) else None


def _first_cancellation(*errors: BaseException | None) -> KeyboardInterrupt | None:
    for error in errors:
        cause = _cancellation_cause(error)
        if cause is not None:
            return cause
    return None


def _font_failure_reason(error: BaseException) -> str:
    if isinstance(error, ToolError):
        return error.reason
    if isinstance(error, KeyboardInterrupt):
        return "使用者取消了命令"
    return f"{type(error).__name__}"


def _font_temporary_sites(*errors: BaseException) -> tuple[str, ...]:
    """从共享原子写错误指向的目标确认仍存在的字体临时文件。"""

    sites: list[str] = []
    for error in errors:
        if not isinstance(error, ToolError):
            continue
        try:
            target = Path(error.object_name)
            temporary = (
                target
                if target.name.endswith(".att-font.tmp")
                else target.with_name(f".{target.name}.att-font.tmp")
            )
            temporary.lstat()
        except (OSError, ValueError):
            continue
        text = str(temporary)
        if text not in sites:
            sites.append(text)
    return tuple(sites)


def _with_font_temporary_facts(impact: str, *errors: BaseException) -> str:
    sites = _font_temporary_sites(*errors)
    if not sites:
        return impact
    return f"{impact}；字体事务临时现场保留于 {' 与 '.join(sites)}"


def _best_effort_recovery_status(
    state: Path,
    *,
    plan: FontTransactionPlan | None = None,
    binding: FontStateBinding | None = None,
) -> tuple[bool, BaseException | None]:
    try:
        if plan is None or binding is None:
            _write_state_status(state, "recovery_required")
        else:
            _write_bound_state_status(
                plan,
                state=state,
                binding=binding,
                expected_statuses=frozenset({"prepared", "applied"}),
                status="recovery_required",
            )
    except BaseException as error:  # noqa: BLE001 - 返回状态事实并保留取消原因。
        try:
            recorded = _read_state_status(state) == "recovery_required"
        except BaseException as inspection_error:  # noqa: BLE001 - 状态检查本身也可能被取消。
            return False, inspection_error if _cancellation_cause(inspection_error) else error
        return recorded, error
    return True, None


def _validate_mutation_before_apply(game_root: Path, mutation: ByteMutation) -> None:
    target = _target(game_root, mutation.relative_path)
    if mutation.original is None:
        if target.exists():
            fail(
                mutation.relative_path,
                "apply 前目标由其他进程建立",
                "重新 inspect 并使用新的 state 目录",
            )
    elif not target.is_file() or sha256_bytes(target.read_bytes()) != sha256_bytes(mutation.original):
        fail(mutation.relative_path, "apply 前游戏文件已经变化", "重新 inspect，不要套用旧计划")


def _validate_game_before_apply(plan: FontTransactionPlan) -> None:
    for mutation in plan.mutations:
        _validate_mutation_before_apply(plan.game_root, mutation)


def verify_font_plan_source(plan: FontTransactionPlan) -> None:
    """在游戏任务锁内确认 plan 仍对应当前源游戏字节。"""

    _validate_game_before_apply(plan)


def _verify_applied_game(plan: FontTransactionPlan) -> None:
    for mutation in plan.mutations:
        target = _target(plan.game_root, mutation.relative_path)
        try:
            body = target.read_bytes() if target.is_file() else None
        except OSError as error:
            raise ToolError(
                object_name=str(target),
                reason=f"apply 最终验收无法读取 replacement（{type(error).__name__}）",
                impact="字体事务已经执行，但最终游戏字节未通过验收；state 与结果文件保留",
                help_text="停止使用当前游戏副本，保留游戏与 state 并核对该自然路径",
            ) from None
        if body != mutation.replacement:
            raise ToolError(
                object_name=str(target),
                reason="apply 最终验收发现 replacement 字节不一致",
                impact="字体事务已经执行，但最终游戏字节未通过验收；state 与结果文件保留",
                help_text="停止使用当前游戏副本，保留游戏与 state 并核对该自然路径",
            )


def _state_change_after_apply_attempt(
    error: FontStateIntegrityError,
    *,
    game_root: Path,
    attempted_count: int,
    game_restored: bool,
) -> ToolError:
    if game_restored:
        game_impact = (
            "目标游戏尚未开始字体写入"
            if attempted_count == 0
            else f"本次 apply 尝试过 {attempted_count} 项游戏写入，现已恢复为 apply 前字节"
        )
    else:
        game_impact = "本次 apply 已修改游戏文件，当前游戏终态无法确认"
    return ToolError(
        object_name=str(game_root),
        reason=error.reason,
        impact=f"{game_impact}；{error.impact}",
        help_text=(
            "停止使用当前游戏副本，保留游戏目录和现有 state 路径；"
            "根据原 state 的实际残留位置人工核对后再决定恢复"
        ),
    )


def apply_font_plan(
    plan: FontTransactionPlan,
    *,
    state: Path,
    binding: FontStateBinding,
) -> None:
    """在 state 已发布后执行计划；任一失败时恢复全部原始字节。"""

    try:
        _verify_font_state(
            plan,
            state=state,
            binding=binding,
            statuses=frozenset({"prepared"}),
            marker=None,
            full=True,
        )
    except FontStateIntegrityError as error:
        raise _state_change_after_apply_attempt(
            error,
            game_root=plan.game_root,
            attempted_count=0,
            game_restored=True,
        ) from None
    _validate_game_before_apply(plan)
    attempted: list[ByteMutation] = []
    try:
        for mutation in plan.mutations:
            _verify_font_state(
                plan,
                state=state,
                binding=binding,
                statuses=frozenset({"prepared"}),
                marker=None,
                full=False,
            )
            _validate_mutation_before_apply(plan.game_root, mutation)
            target = _target(plan.game_root, mutation.relative_path)
            attempted.append(mutation)
            _atomic_write_bytes(target, mutation.replacement, expect_missing=mutation.original is None)
            _verify_font_state(
                plan,
                state=state,
                binding=binding,
                statuses=frozenset({"prepared"}),
                marker=None,
                full=False,
            )
        _write_bound_state_status(
            plan,
            state=state,
            binding=binding,
            expected_statuses=frozenset({"prepared"}),
            status="applied",
        )
    except BaseException as primary:  # noqa: BLE001 - 任何写入失败都必须进行整个计划回滚。
        rollback_failures = _rollback_to_original(plan.game_root, attempted)
        if not rollback_failures:
            if isinstance(primary, FontStateIntegrityError):
                raise _state_change_after_apply_attempt(
                    primary,
                    game_root=plan.game_root,
                    attempted_count=len(attempted),
                    game_restored=True,
                ) from None
            try:
                _write_bound_state_status(
                    plan,
                    state=state,
                    binding=binding,
                    expected_statuses=frozenset({"prepared", "applied"}),
                    status="rolled_back",
                )
            except BaseException as status_error:  # noqa: BLE001 - 原字节已恢复，但机器状态无法确认。
                cancellation = _first_cancellation(primary, status_error)
                if cancellation is not None:
                    raise ToolCancelledError(
                        object_name=str(state / "status.json"),
                        reason=(
                            "使用者取消了字体 apply；游戏回滚完成，state 状态更新发生："
                            f"{_font_failure_reason(status_error)}"
                        ),
                        impact=_with_font_temporary_facts(
                            "游戏文件已经恢复为 apply 前字节；state 状态保留在可核对的实际值",
                            primary,
                            status_error,
                        ),
                        help_text="核对 status.json 后重新 inspect/apply",
                        cause=cancellation,
                    ) from None
                if isinstance(status_error, FontStateIntegrityError):
                    raise _state_change_after_apply_attempt(
                        status_error,
                        game_root=plan.game_root,
                        attempted_count=len(attempted),
                        game_restored=True,
                    ) from None
                raise ToolError(
                    object_name=str(state / "status.json"),
                    reason=f"字体事务失败后状态无法更新：{_font_failure_reason(status_error)}",
                    impact=_with_font_temporary_facts(
                        "游戏文件已经恢复为 apply 前字节；state 状态仍可能显示 prepared",
                        primary,
                        status_error,
                    ),
                    help_text="保留 state 与游戏目录，处理状态文件权限后重新 inspect/apply",
                ) from None
            cancellation = _first_cancellation(primary)
            if cancellation is not None:
                raise ToolCancelledError(
                    object_name=str(plan.game_root),
                    reason=f"使用者取消了字体 apply：{_font_failure_reason(primary)}",
                    impact=_with_font_temporary_facts(
                        "本次已经写入的文件全部恢复为 apply 前字节；state/status.json 已记录 rolled_back",
                        primary,
                    ),
                    help_text="重新 inspect 当前游戏后，使用新的 state 目录再次 apply",
                    cause=cancellation,
                ) from None
            raise ToolError(
                object_name=str(plan.game_root),
                reason=f"字体事务写入失败：{_font_failure_reason(primary)}",
                impact=_with_font_temporary_facts(
                    "本次已经写入的文件全部恢复为 apply 前的原始字节；state 现场保留",
                    primary,
                ),
                help_text="处理磁盘空间、权限或占用后重新 inspect/apply",
            ) from None
        cancellation = _first_cancellation(primary, *rollback_failures)
        if isinstance(primary, FontStateIntegrityError):
            if cancellation is not None:
                raise ToolCancelledError(
                    object_name=str(plan.game_root),
                    reason=(
                        f"使用者取消了字体 apply：{_font_failure_reason(primary)}；"
                        f"另有 {len(rollback_failures)} 项游戏回滚无法确认"
                    ),
                    impact=_with_font_temporary_facts(
                        f"当前游戏终态无法确认；{primary.impact}",
                        primary,
                        *rollback_failures,
                    ),
                    help_text=(
                        "立即停止使用该游戏目录，保留当前 state 路径并定位原恢复 state 的实际残留位置"
                    ),
                    cause=cancellation,
                ) from None
            raise ToolError(
                object_name=str(plan.game_root),
                reason=(f"{primary.reason}；另有 {len(rollback_failures)} 项游戏回滚无法确认"),
                impact=_with_font_temporary_facts(
                    f"本次 apply 已修改游戏文件，当前游戏终态无法确认；{primary.impact}",
                    primary,
                    *rollback_failures,
                ),
                help_text=("立即停止使用该游戏目录，保留当前 state 路径并定位原恢复 state 的实际残留位置"),
            ) from None
        status_recorded, status_error = _best_effort_recovery_status(
            state,
            plan=plan,
            binding=binding,
        )
        cancellation = cancellation or _first_cancellation(status_error)
        if cancellation is not None:
            raise ToolCancelledError(
                object_name=str(plan.game_root),
                reason=(
                    f"使用者取消了字体 apply：{_font_failure_reason(primary)}；"
                    f"{len(rollback_failures)} 项回滚无法确认"
                ),
                impact=_with_font_temporary_facts(
                    (
                        "无法确认目标游戏状态；state/status.json 已记录 recovery_required"
                        if status_recorded
                        else "无法确认目标游戏状态；state/status.json 未能确认 recovery_required"
                    ),
                    primary,
                    *rollback_failures,
                    *(() if status_error is None else (status_error,)),
                ),
                help_text="立即停止使用该游戏目录，按 state/manifest.json 的自然路径恢复",
                cause=cancellation,
            ) from None
        raise ToolError(
            object_name=str(plan.game_root),
            reason=(
                f"字体事务写入失败：{_font_failure_reason(primary)}；{len(rollback_failures)} 项回滚无法确认"
            ),
            impact=_with_font_temporary_facts(
                (
                    "无法确认目标游戏状态；state/status.json 已标记 recovery_required"
                    if status_recorded
                    else "无法确认目标游戏状态，且 state/status.json 无法标记 recovery_required"
                ),
                primary,
                *rollback_failures,
                *(() if status_error is None else (status_error,)),
            ),
            help_text="立即停止使用该游戏目录，按 state/manifest.json 的自然路径恢复；不要重试 apply",
        ) from None


def verify_applied_font_plan(
    plan: FontTransactionPlan,
    *,
    state: Path,
    binding: FontStateBinding,
    applied_marker: bytes,
) -> None:
    """在发布结果后复核 applied state 和全部 replacement 字节。"""

    _verify_font_state_binding(
        plan,
        state=state,
        binding=binding,
        expected_status="applied",
        applied_marker=applied_marker,
    )
    _verify_applied_game(plan)
    _verify_font_state_binding(
        plan,
        state=state,
        binding=binding,
        expected_status="applied",
        applied_marker=applied_marker,
    )


def write_font_apply_marker(
    plan: FontTransactionPlan,
    *,
    state: Path,
    binding: FontStateBinding,
    mutation_count: int,
    confirmed_reference_count: int,
) -> bytes:
    """在同一 canonical state 内原子写入并复核 applied 标记。"""

    marker_body = (
        json.dumps(
            {
                "applied": True,
                "mutation_count": mutation_count,
                "confirmed_reference_count": confirmed_reference_count,
            },
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")
    _verify_font_state(
        plan,
        state=state,
        binding=binding,
        statuses=frozenset({"applied"}),
        marker=None,
        full=True,
    )
    try:
        _atomic_write_bytes(state / "applied.json", marker_body, expect_missing=True)
    except BaseException as error:  # noqa: BLE001 - 失败后仍须核验 state 是否保持绑定。
        try:
            _verify_font_state(
                plan,
                state=state,
                binding=binding,
                statuses=frozenset({"applied"}),
                marker=marker_body,
                full=True,
            )
        except FontStateIntegrityError as published_error:
            try:
                _verify_font_state(
                    plan,
                    state=state,
                    binding=binding,
                    statuses=frozenset({"applied"}),
                    marker=None,
                    full=True,
                )
            except FontStateIntegrityError:
                raise published_error from None
            marker_impact = "字体替换已生效；已绑定 state 仍完整，applied 标记尚未建立"
        else:
            if isinstance(error, OutputPublishedError):
                raise error from None
            marker_impact = "字体替换与 applied 标记均已生效；标记临时文件的清理终态无法确认"
        cancellation = _first_cancellation(error)
        if cancellation is not None:
            raise ToolCancelledError(
                object_name=str(state / "applied.json"),
                reason="使用者取消了 applied 标记写入",
                impact=marker_impact,
                help_text="保留游戏与 state，核对 applied.json 后决定是否发布 Review JSON",
                cause=cancellation,
            ) from None
        raise ToolError(
            object_name=str(state / "applied.json"),
            reason=f"applied 标记写入失败（{type(error).__name__}）",
            impact=marker_impact,
            help_text="保留游戏与 state，处理权限、占用或磁盘空间后人工记录结果",
        ) from None
    _verify_font_state(
        plan,
        state=state,
        binding=binding,
        statuses=frozenset({"applied"}),
        marker=marker_body,
        full=True,
    )
    return marker_body


def _ordinary_file_identity(path: Path) -> tuple[int, int, int, int]:
    metadata = path.lstat()
    file_attributes = getattr(metadata, "st_file_attributes", 0)
    if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or bool(file_attributes & 0x400):
        raise OSError("路径不是普通文件")
    if metadata.st_ino == 0:
        raise OSError("文件系统没有提供稳定文件身份")
    return metadata.st_dev, metadata.st_ino, metadata.st_size, metadata.st_mtime_ns


def _restore_state_error(state: Path, reason: str) -> NoReturn:
    raise ToolError(
        object_name=str(Path(os.path.abspath(state))),
        reason=reason,
        impact="字体 restore 已停止；state 保留在当前可核对状态",
        help_text="保留游戏与 state，恢复 apply 建立的完整 state 后重新运行 restore",
    )


def _require_restore_state_identity(state: Path, binding: _RestoreStateBinding) -> None:
    if Path(os.path.abspath(state)) != binding.path:
        _restore_state_error(state, "字体 state 路径与 restore 开始时绑定的路径不一致")
    try:
        identity = _directory_identity(state)
        manifest_identity = _ordinary_file_identity(state / "manifest.json")
    except OSError:
        _restore_state_error(state, "字体 state 在 restore 期间被移走、替换或改变了路径类型")
    if identity != binding.identity or manifest_identity != binding.manifest_identity:
        _restore_state_error(state, "字体 state 或 manifest 在 restore 期间发生变化")


def _verify_restore_manifest(state: Path, binding: _RestoreStateBinding) -> None:
    _require_restore_state_identity(state, binding)
    manifest_path = state / "manifest.json"
    try:
        body = manifest_path.read_bytes()
    except OSError:
        _restore_state_error(state, "字体 state 的 manifest 在 restore 期间无法读取")
    if sha256_bytes(body) != binding.manifest_sha256:
        _restore_state_error(state, "字体 state 的 manifest 在 restore 期间发生变化")
    _require_restore_state_identity(state, binding)


def _restore_entry(manifest_path: Path, item: object) -> _RestoreEntry:
    if not isinstance(item, dict):
        fail(str(manifest_path), "字体 state 清单含无效条目", "恢复 apply 建立的完整 state")
    value = cast(dict[object, object], item)
    relative = value.get("path")
    before_file = value.get("before_file")
    before_sha256 = value.get("before_sha256")
    after_file = value.get("after_file")
    after_sha256 = value.get("after_sha256")
    if not isinstance(relative, str) or not isinstance(after_file, str) or not isinstance(after_sha256, str):
        fail(str(manifest_path), "字体 state 条目缺少自然路径或 after 快照", "恢复 apply 建立的完整 state")
    if (before_file is None) != (before_sha256 is None) or (
        before_file is not None and (not isinstance(before_file, str) or not isinstance(before_sha256, str))
    ):
        fail(str(manifest_path), "字体 state 条目的 before 快照结构无效", "恢复 apply 建立的完整 state")
    return _RestoreEntry(
        relative_path=relative,
        before_file=before_file,
        before_sha256=cast(str | None, before_sha256),
        after_file=after_file,
        after_sha256=after_sha256,
    )


def _load_state(state: Path) -> tuple[Path, tuple[_RestoreEntry, ...], _RestoreStateBinding]:
    natural_state = Path(os.path.abspath(state))
    manifest_path = natural_state / "manifest.json"
    try:
        state_identity = _directory_identity(natural_state)
        manifest_identity = _ordinary_file_identity(manifest_path)
        manifest_body = manifest_path.read_bytes()
        if _ordinary_file_identity(manifest_path) != manifest_identity:
            raise OSError("manifest 在读取期间发生变化")
        if _directory_identity(natural_state) != state_identity:
            raise OSError("state 在读取期间发生变化")
        value = cast(object, json.loads(manifest_body.decode("utf-8")))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(
            str(manifest_path),
            f"字体 state 清单无法稳定读取（{type(error).__name__}）",
            "保留现场并恢复 apply 建立的完整 state",
        )
    if not isinstance(value, dict):
        fail(str(manifest_path), "字体 state 清单结构无效", "恢复 apply 建立的完整 state")
    typed_value = cast(dict[object, object], value)
    game_root_value = typed_value.get("game_root")
    entries_value = typed_value.get("entries")
    if not isinstance(game_root_value, str) or not isinstance(entries_value, list):
        fail(str(manifest_path), "字体 state 清单结构无效", "恢复 apply 建立的完整 state")
    entries = tuple(_restore_entry(manifest_path, item) for item in cast(list[object], entries_value))
    binding = _RestoreStateBinding(
        path=natural_state,
        identity=state_identity,
        manifest_identity=manifest_identity,
        manifest_sha256=sha256_bytes(manifest_body),
    )
    _verify_restore_manifest(natural_state, binding)
    return Path(game_root_value).resolve(strict=False), entries, binding


def _state_blob(
    state: Path,
    binding: _RestoreStateBinding,
    name: str,
    expected_sha: str,
) -> bytes:
    relative = PurePosixPath(name)
    if not relative.parts or relative.is_absolute() or ".." in relative.parts or "\\" in name or ":" in name:
        fail(name, "字体 state 快照路径越界", "恢复 apply 建立的完整 state")
    _require_restore_state_identity(state, binding)
    for parent in relative.parents:
        if parent != PurePosixPath("."):
            try:
                _directory_identity(state.joinpath(*parent.parts))
            except OSError:
                _restore_state_error(
                    state, f"字体 state 快照目录 {parent.as_posix()} 在 restore 期间发生变化"
                )
    path = state.joinpath(*relative.parts)
    try:
        identity = _ordinary_file_identity(path)
        body = path.read_bytes()
        if _ordinary_file_identity(path) != identity:
            raise OSError("快照在读取期间发生变化")
    except OSError:
        _restore_state_error(state, f"字体 state 快照 {name} 缺失、被替换或无法稳定读取")
    if sha256_bytes(body) != expected_sha:
        _restore_state_error(state, f"字体 state 快照 {name} 的摘要不一致")
    _require_restore_state_identity(state, binding)
    return body


def _restore_entry_material(
    game_root: Path,
    state: Path,
    binding: _RestoreStateBinding,
    entry: _RestoreEntry,
) -> tuple[Path, bytes | None, bytes]:
    target = _target(game_root, entry.relative_path)
    after = _state_blob(state, binding, entry.after_file, entry.after_sha256)
    before = (
        None
        if entry.before_file is None or entry.before_sha256 is None
        else _state_blob(state, binding, entry.before_file, entry.before_sha256)
    )
    _require_restore_state_identity(state, binding)
    return target, before, after


def _raise_restore_unknown(
    primary: BaseException,
    rollback_failures: Sequence[BaseException],
    *,
    state: Path,
    binding: _RestoreStateBinding,
) -> NoReturn:
    status_recorded, status_error = _best_effort_bound_recovery_status(state, binding)
    cancellation = _first_cancellation(primary, *rollback_failures, status_error)
    if cancellation is not None:
        raise ToolCancelledError(
            object_name="字体 restore 事务",
            reason=(
                f"使用者取消了字体 restore：{_font_failure_reason(primary)}；"
                f"恢复本次 restore 前状态另有 {len(rollback_failures)} 项失败"
            ),
            impact=_with_font_temporary_facts(
                (
                    "无法确认目标游戏状态；state/status.json 已记录 recovery_required"
                    if status_recorded
                    else "无法确认目标游戏状态；state/status.json 未能确认 recovery_required"
                ),
                primary,
                *rollback_failures,
                *(() if status_error is None else (status_error,)),
            ),
            help_text="立即停止使用该游戏目录，按 manifest 的自然路径人工核对",
            cause=cancellation,
        ) from None
    raise ToolError(
        object_name="字体 restore 事务",
        reason=(
            f"恢复失败：{_font_failure_reason(primary)}；"
            f"恢复本次 restore 前状态另有 {len(rollback_failures)} 项失败"
        ),
        impact=_with_font_temporary_facts(
            (
                "无法确认目标游戏状态；state/status.json 已标记 recovery_required"
                if status_recorded
                else "无法确认目标游戏状态，且 state/status.json 无法标记 recovery_required"
            ),
            primary,
            *rollback_failures,
            *(() if status_error is None else (status_error,)),
        ),
        help_text="立即停止使用该游戏目录，按 manifest 的自然路径人工核对；不要重试",
    ) from None


def _restore_target_position(target: Path, before: bytes | None, after: bytes) -> str:
    """即时读取目标并返回 before、after 或 third。"""

    try:
        present = target.exists() or target.is_symlink()
        current = target.read_bytes() if target.is_file() else None
    except OSError as error:
        raise ToolError(
            object_name=str(target),
            reason=f"restore 无法读取当前文件（{type(error).__name__}）",
            impact="当前目标尚未由本次 restore 覆盖；state 保留",
            help_text="恢复该自然路径的读取权限后重新运行 restore",
        ) from None
    if before is None:
        if not present:
            return "before"
    elif current == before:
        return "before"
    if current == after:
        return "after"
    return "third"


def _require_known_restore_position(
    target: Path,
    relative: str,
    before: bytes | None,
    after: bytes,
) -> str:
    position = _restore_target_position(target, before, after)
    if position == "third":
        fail(
            relative,
            "restore 前文件既不等于记录的 before，也不等于 after",
            "保留当前文件与 state，人工判断变化来源；工具不会覆盖第三种字节",
        )
    return position


def _verify_restored_targets(
    game_root: Path,
    state: Path,
    binding: _RestoreStateBinding,
    entries: Sequence[_RestoreEntry],
) -> None:
    """逐项加载 state 快照并确认游戏位于 before，不长期保留快照正文。"""

    _verify_restore_manifest(state, binding)
    for entry in entries:
        target, before, after = _restore_entry_material(game_root, state, binding, entry)
        if _restore_target_position(target, before, after) != "before":
            raise ToolError(
                object_name=str(target),
                reason="restore 后 before 字节不一致",
                impact="无法确认 restore 结果；state 保留",
                help_text="停止使用目标游戏并按 manifest 核对",
            )
    _verify_restore_manifest(state, binding)


def _record_bound_restore_status(
    state: Path,
    binding: _RestoreStateBinding,
    status: str,
) -> None:
    _verify_restore_manifest(state, binding)
    if _read_state_status(state) != status:
        _write_state_status(state, status)
    if _read_state_status(state) != status:
        raise OSError(f"restore 状态写入后未读回 {status}")
    _verify_restore_manifest(state, binding)


def _best_effort_bound_recovery_status(
    state: Path,
    binding: _RestoreStateBinding,
) -> tuple[bool, BaseException | None]:
    try:
        _record_bound_restore_status(state, binding, "recovery_required")
    except BaseException as error:  # noqa: BLE001 - 返回精确机器状态并保留取消原因。
        try:
            _require_restore_state_identity(state, binding)
            recorded = _read_state_status(state) == "recovery_required"
        except BaseException as inspection_error:  # noqa: BLE001 - 检查本身也可能取消。
            return False, inspection_error if _first_cancellation(inspection_error) else error
        return recorded, error
    return True, None


def _restore_failure_reason(error: BaseException) -> str:
    if isinstance(error, ToolError):
        return error.reason
    if isinstance(error, KeyboardInterrupt):
        return "使用者取消了字体 restore"
    return f"字体 restore 最终处理发生 {type(error).__name__}"


def _settle_restore_failure(
    primary: BaseException,
    *,
    game_root: Path,
    state: Path,
    binding: _RestoreStateBinding,
    entries: Sequence[_RestoreEntry],
) -> NoReturn:
    """最终处理失败后，再次形成 restored 或 recovery_required 的可观察终态。"""

    try:
        _verify_restored_targets(game_root, state, binding, entries)
        _record_bound_restore_status(state, binding, "restored")
        _verify_restored_targets(game_root, state, binding, entries)
    except BaseException as terminal_error:  # noqa: BLE001 - 必须把真实游戏和 state 终态写入诊断。
        status_recorded, status_error = _best_effort_bound_recovery_status(state, binding)
        cancellation = _first_cancellation(primary, terminal_error, status_error)
        reason = (
            f"{_restore_failure_reason(primary)}；最终字节或状态复核发生 "
            f"{_restore_failure_reason(terminal_error)}"
        )
        impact = _with_font_temporary_facts(
            (
                "无法确认目标游戏状态；state/status.json 已记录 recovery_required"
                if status_recorded
                else "无法确认目标游戏状态；state/status.json 未能确认 recovery_required"
            ),
            primary,
            terminal_error,
            *(() if status_error is None else (status_error,)),
        )
        details = {
            "object_name": str(state),
            "reason": reason,
            "impact": impact,
            "help_text": "立即停止使用该游戏目录，按 manifest 的自然路径人工核对",
        }
        if cancellation is not None:
            raise ToolCancelledError(**details, cause=cancellation) from None
        raise ToolError(**details) from None
    cancellation = _first_cancellation(primary)
    details = {
        "object_name": str(state),
        "reason": _restore_failure_reason(primary),
        "impact": _with_font_temporary_facts(
            "游戏文件已经恢复为 apply 前字节；state/status.json 已记录 restored",
            primary,
        ),
        "help_text": "保留游戏与 state；需要结果 JSON 时可再次运行 restore",
    }
    if cancellation is not None:
        raise ToolCancelledError(**details, cause=cancellation) from None
    raise ToolError(**details) from None


def restore_font_state(*, game_root: Path, state: Path) -> int:
    """接受每项处于 before 或 after；只恢复 after 项，拒绝第三种字节。"""

    try:
        manifest_root, entries, binding = _load_state(state)
        _read_state_status(state)
        if manifest_root != game_root:
            fail(
                str(state), "state 记录的游戏根与本次 --game 不一致", "对原 apply 使用的同一游戏目录 restore"
            )
        for entry in entries:
            target, before, after = _restore_entry_material(game_root, state, binding, entry)
            _require_known_restore_position(target, entry.relative_path, before, after)
        _verify_restore_manifest(state, binding)
    except BaseException as error:
        cancellation = _first_cancellation(error)
        if cancellation is not None:
            raise ToolCancelledError(
                object_name=str(state),
                reason="使用者在字体 restore 首次扫描期间取消了命令",
                impact="目标游戏尚未开始恢复写入；state 未由本次 restore 修改",
                help_text="保留游戏与 state，可直接再次运行 restore",
                cause=cancellation,
            ) from None
        raise
    attempted: list[int] = []
    try:
        for index in range(len(entries) - 1, -1, -1):
            entry = entries[index]
            target, before, after = _restore_entry_material(game_root, state, binding, entry)
            position = _require_known_restore_position(target, entry.relative_path, before, after)
            if position == "before":
                continue
            attempted.append(index)
            if before is None:
                target.unlink()
            else:
                _atomic_write_bytes(target, before, expect_missing=False)
        _verify_restore_manifest(state, binding)
    except BaseException as primary:  # noqa: BLE001 - restore 必须回到本次操作前可确认的逐项状态。
        rollback_failures: list[BaseException] = []
        for index in reversed(attempted):
            try:
                entry = entries[index]
                target, before, after = _restore_entry_material(game_root, state, binding, entry)
                if not target.exists():
                    if before is not None:
                        raise OSError("已尝试恢复的文件意外缺失")
                    _atomic_write_bytes(target, after, expect_missing=True)
                    continue
                if not target.is_file():
                    raise OSError("已尝试恢复的目标不再是普通文件")
                current_sha = sha256_bytes(target.read_bytes())
                if current_sha == sha256_bytes(after):
                    continue
                if before is None or current_sha != sha256_bytes(before):
                    raise OSError("已尝试恢复的文件出现未知并发字节")
                _atomic_write_bytes(target, after, expect_missing=False)
            except BaseException as rollback:  # noqa: BLE001 - 每一项都必须尽力回到 restore 前状态。
                rollback_failures.append(rollback)
        if rollback_failures:
            _raise_restore_unknown(primary, rollback_failures, state=state, binding=binding)
        cancellation = _first_cancellation(primary)
        if cancellation is not None:
            raise ToolCancelledError(
                object_name=str(game_root),
                reason=f"使用者取消了字体 restore：{_font_failure_reason(primary)}",
                impact=_with_font_temporary_facts(
                    "已恢复并核验本次 restore 前的逐项 before/after 状态；state 保留",
                    primary,
                ),
                help_text="核对游戏与 state 后再次运行 restore",
                cause=cancellation,
            ) from None
        raise ToolError(
            object_name=str(game_root),
            reason=f"字体 restore 写入失败：{_font_failure_reason(primary)}",
            impact=_with_font_temporary_facts(
                "已恢复并核验本次 restore 前的逐项 before/after 状态；state 保留",
                primary,
            ),
            help_text="处理权限、占用或磁盘错误后再次 restore",
        ) from None
    try:
        _verify_restored_targets(game_root, state, binding, entries)
        _record_bound_restore_status(state, binding, "restored")
        _verify_restored_targets(game_root, state, binding, entries)
    except BaseException as error:  # noqa: BLE001 - 收束为 restored 或 recovery_required 后再公开失败。
        _settle_restore_failure(
            error,
            game_root=game_root,
            state=state,
            binding=binding,
            entries=entries,
        )
    return len(attempted)


def verify_restored_font_state(*, game_root: Path, state: Path) -> None:
    """在发布 restore 结果后复核 state 和每一项 before 字节。"""

    manifest_root, entries, binding = _load_state(state)
    if manifest_root != game_root:
        fail(str(state), "state 记录的游戏根与本次 --game 不一致", "对原 apply 使用的同一游戏目录 restore")
    if _read_state_status(state) != "restored":
        raise ToolError(
            object_name=str(state / "status.json"),
            reason="restore 最终验收发现 state 状态不是 restored",
            impact="restore 主流程已经执行，但最终 state 未通过验收；游戏与结果文件保留",
            help_text="保留游戏与 state，核对 status.json 和 manifest.json",
        )
    try:
        _verify_restored_targets(game_root, state, binding, entries)
    except ToolError as error:
        raise ToolError(
            object_name=error.object_name,
            reason=error.reason,
            impact="restore 主流程已经执行，但最终游戏字节未通过验收，或 state 已变化；结果文件保留",
            help_text="停止使用当前游戏副本，保留游戏、state 与结果文件并核对自然路径",
        ) from None
    if _read_state_status(state) != "restored":
        raise ToolError(
            object_name=str(state / "status.json"),
            reason="restore 最终验收期间 state 状态发生变化",
            impact="最终 state 未通过验收；游戏与结果文件保留",
            help_text="保留游戏与 state，核对 status.json 和 manifest.json",
        )
