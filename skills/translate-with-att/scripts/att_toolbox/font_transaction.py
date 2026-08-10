"""RPG Maker 字体替换的前后字节快照、apply 回滚与 drift-safe restore。"""

from __future__ import annotations

import hashlib
import json
import os
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import NoReturn, Protocol, cast

from att_skill_tools import ToolError, fail

_STATE_STATUSES = frozenset({"prepared", "applied", "rolled_back", "recovery_required", "restored"})


@dataclass(frozen=True, slots=True)
class ByteMutation:
    relative_path: str
    original: bytes | None
    replacement: bytes


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
    files["status.json"] = (
        json.dumps(
            {"status": "prepared"},
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )
    return files


def _atomic_write_bytes(target: Path, body: bytes, *, expect_missing: bool) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(f".{target.name}.att-font.tmp")
    if temporary.exists():
        raise OSError("固定字体事务临时文件已存在")
    with temporary.open("xb") as handle:
        handle.write(body)
        handle.flush()
        os.fsync(handle.fileno())
    try:
        if expect_missing:
            os.link(temporary, target)
            temporary.unlink()
        else:
            os.replace(temporary, target)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


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
    target = game_root.joinpath(*relative.parts).resolve(strict=False)
    try:
        target.relative_to(game_root)
    except ValueError:
        fail(relative_text, "字体事务路径解析后超出游戏根", "不要编辑事务清单；重新 inspect/apply")
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
    body = (json.dumps({"status": status}, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode(
        "utf-8"
    )
    _atomic_write_bytes(status_path, body, expect_missing=False)


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


def _best_effort_recovery_status(state: Path) -> bool:
    try:
        _write_state_status(state, "recovery_required")
    except BaseException:  # noqa: BLE001 - 主故障优先，返回值明确标记状态文件也失败。
        return False
    return True


def apply_font_plan(plan: FontTransactionPlan, *, state: Path) -> None:
    """在 state 已发布后执行计划；任一失败时恢复全部原始字节。"""

    if _read_state_status(state) != "prepared":
        fail(str(state), "字体 state 不是尚未执行的 prepared 状态", "每次 apply 使用新建的 state 目录")
    for mutation in plan.mutations:
        target = _target(plan.game_root, mutation.relative_path)
        if mutation.original is None:
            if target.exists():
                fail(
                    mutation.relative_path, "apply 前目标由其他进程建立", "重新 inspect 并使用新的 state 目录"
                )
        elif not target.is_file() or sha256_bytes(target.read_bytes()) != sha256_bytes(mutation.original):
            fail(mutation.relative_path, "apply 前游戏文件已经变化", "重新 inspect，不要套用旧计划")
    attempted: list[ByteMutation] = []
    try:
        for mutation in plan.mutations:
            target = _target(plan.game_root, mutation.relative_path)
            attempted.append(mutation)
            _atomic_write_bytes(target, mutation.replacement, expect_missing=mutation.original is None)
        _write_state_status(state, "applied")
    except BaseException as primary:  # noqa: BLE001 - 任何写入失败都必须进行整个计划回滚。
        rollback_failures = _rollback_to_original(plan.game_root, attempted)
        if not rollback_failures:
            try:
                _write_state_status(state, "rolled_back")
            except BaseException as status_error:  # noqa: BLE001 - 原字节已恢复，但机器状态无法确认。
                raise ToolError(
                    object_name=str(state / "status.json"),
                    reason=f"字体事务失败后状态无法更新（{type(status_error).__name__}）",
                    impact="游戏文件已经恢复为 apply 前字节；state 状态仍可能显示 prepared",
                    help_text="保留 state 与游戏目录，处理状态文件权限后重新 inspect/apply",
                ) from None
            raise ToolError(
                object_name=str(plan.game_root),
                reason=f"字体事务写入失败（{type(primary).__name__}）",
                impact="本次已经写入的文件全部恢复为 apply 前的原始字节；state 现场保留",
                help_text="处理磁盘空间、权限或占用后重新 inspect/apply",
            ) from None
        status_recorded = _best_effort_recovery_status(state)
        raise ToolError(
            object_name=str(plan.game_root),
            reason=(f"字体事务写入失败（{type(primary).__name__}），{len(rollback_failures)} 项回滚无法确认"),
            impact=(
                "无法确认目标游戏状态；state/status.json 已标记 recovery_required"
                if status_recorded
                else "无法确认目标游戏状态，且 state/status.json 无法标记 recovery_required"
            ),
            help_text="立即停止使用该游戏目录，按 state/manifest.json 的自然路径恢复；不要重试 apply",
        ) from None


def _load_state(state: Path) -> tuple[Path, list[dict[str, object]]]:
    manifest_path = state / "manifest.json"
    try:
        value = cast(object, json.loads(manifest_path.read_text(encoding="utf-8")))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(
            str(manifest_path),
            f"字体 state 清单无法读取（{type(error).__name__}）",
            "保留现场并恢复完整 state",
        )
    if not isinstance(value, dict):
        fail(str(manifest_path), "字体 state 清单结构无效", "不要编辑 state；恢复 apply 建立的完整目录")
    typed_value = cast(dict[object, object], value)
    game_root_value = typed_value.get("game_root")
    entries_value = typed_value.get("entries")
    if not isinstance(game_root_value, str) or not isinstance(entries_value, list):
        fail(str(manifest_path), "字体 state 清单结构无效", "不要编辑 state；恢复 apply 建立的完整目录")
    entries: list[dict[str, object]] = []
    for item in cast(list[object], entries_value):
        if not isinstance(item, dict):
            fail(str(manifest_path), "字体 state 清单含无效条目", "不要编辑 state")
        entries.append({str(key): member for key, member in cast(dict[object, object], item).items()})
    return Path(game_root_value).resolve(strict=False), entries


def _state_blob(state: Path, name: object, expected_sha: object) -> bytes:
    if not isinstance(name, str) or not isinstance(expected_sha, str):
        fail(str(state / "manifest.json"), "字体 state 条目缺少快照文件或摘要", "不要编辑 state")
    relative = PurePosixPath(name)
    if relative.is_absolute() or ".." in relative.parts or "\\" in name or ":" in name:
        fail(name, "字体 state 快照路径越界", "不要编辑 state")
    path = state.joinpath(*relative.parts)
    try:
        body = path.read_bytes()
    except OSError:
        fail(str(path), "字体 state 快照缺失", "恢复完整 state 后重试")
    if sha256_bytes(body) != expected_sha:
        fail(str(path), "字体 state 快照摘要不一致", "恢复未修改的完整 state；不要继续 restore")
    return body


def _raise_restore_unknown(
    primary: BaseException,
    rollback_failures: Sequence[BaseException],
    *,
    state: Path,
) -> NoReturn:
    status_recorded = _best_effort_recovery_status(state)
    raise ToolError(
        object_name="字体 restore 事务",
        reason=(
            f"恢复失败（{type(primary).__name__}），恢复本次 restore 前状态另有 {len(rollback_failures)} 项失败"
        ),
        impact=(
            "无法确认目标游戏状态；state/status.json 已标记 recovery_required"
            if status_recorded
            else "无法确认目标游戏状态，且 state/status.json 无法标记 recovery_required"
        ),
        help_text="立即停止使用该游戏目录，按 manifest 的自然路径人工核对；不要重试",
    ) from None


def restore_font_state(*, game_root: Path, state: Path) -> int:
    """接受每项处于 before 或 after；只恢复 after 项，拒绝第三种字节。"""

    manifest_root, entries = _load_state(state)
    _read_state_status(state)
    if manifest_root != game_root:
        fail(str(state), "state 记录的游戏根与本次 --game 不一致", "对原 apply 使用的同一游戏目录 restore")
    prepared: list[tuple[Path, bytes | None, bytes, bool]] = []
    for item in entries:
        relative = item.get("path")
        if not isinstance(relative, str):
            fail(str(state / "manifest.json"), "字体 state 条目缺少自然路径", "不要编辑 state")
        target = _target(game_root, relative)
        after = _state_blob(state, item.get("after_file"), item.get("after_sha256"))
        before_file = item.get("before_file")
        before = None if before_file is None else _state_blob(state, before_file, item.get("before_sha256"))
        target_exists = target.exists()
        current_body = target.read_bytes() if target.is_file() else None
        at_before = (
            not target_exists
            if before is None
            else current_body is not None and sha256_bytes(current_body) == sha256_bytes(before)
        )
        at_after = current_body is not None and sha256_bytes(current_body) == sha256_bytes(after)
        if not at_before and not at_after:
            fail(
                relative,
                "restore 前文件既不等于记录的 before，也不等于 after",
                "保留当前文件与 state，人工判断变化来源；工具不会覆盖第三种字节",
            )
        prepared.append((target, before, after, at_after and not at_before))
    attempted: list[tuple[Path, bytes | None, bytes]] = []
    try:
        for target, before, after, needs_restore in reversed(prepared):
            if not needs_restore:
                continue
            attempted.append((target, before, after))
            if before is None:
                target.unlink()
            else:
                _atomic_write_bytes(target, before, expect_missing=not target.exists())
    except BaseException as primary:  # noqa: BLE001 - restore 必须回到本次操作前可确认的逐项状态。
        rollback_failures: list[BaseException] = []
        for target, before, after in reversed(attempted):
            try:
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
            _raise_restore_unknown(primary, rollback_failures, state=state)
        raise ToolError(
            object_name=str(game_root),
            reason=f"字体 restore 写入失败（{type(primary).__name__}）",
            impact="已恢复并核验本次 restore 前的逐项 before/after 状态；state 保留",
            help_text="处理权限、占用或磁盘错误后再次 restore",
        ) from None
    for target, before, _after, _needs_restore in prepared:
        if before is None:
            if target.exists():
                raise ToolError(
                    object_name=str(target),
                    reason="restore 后新建字体仍存在",
                    impact="无法确认 restore 结果；state 保留",
                    help_text="停止使用目标游戏并按 manifest 核对",
                )
        elif not target.is_file() or sha256_bytes(target.read_bytes()) != sha256_bytes(before):
            raise ToolError(
                object_name=str(target),
                reason="restore 后原始字节摘要不一致",
                impact="无法确认 restore 结果；state 保留",
                help_text="停止使用目标游戏并按 manifest 核对",
            )
    try:
        _write_state_status(state, "restored")
    except BaseException as error:  # noqa: BLE001 - 字节已恢复，机器状态写入是独立失败。
        raise ToolError(
            object_name=str(state / "status.json"),
            reason=f"restore 后状态无法更新（{type(error).__name__}）",
            impact="游戏文件已经恢复为 apply 前字节；state 状态未能记录 restored",
            help_text="保留 state 与游戏目录，处理状态文件权限后重新执行 restore",
        ) from None
    return len(attempted)
