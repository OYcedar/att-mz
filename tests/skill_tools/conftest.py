"""随包 Skill 工具测试的进程级运行约束。"""

import sys
from pathlib import Path

import pytest

# 测试直接导入随包模块，也必须保持 Skill 发行资源树不变。
sys.dont_write_bytecode = True


def _skill_bytecode_snapshot() -> tuple[tuple[str, int, int], ...]:
    skill_root = Path(__file__).resolve().parents[2] / "skills"
    entries: list[tuple[str, int, int]] = []
    for path in skill_root.rglob("*"):
        if not path.is_file() or path.suffix not in {".pyc", ".pyo"}:
            continue
        metadata = path.stat()
        entries.append(
            (
                path.relative_to(skill_root).as_posix(),
                metadata.st_size,
                metadata.st_mtime_ns,
            )
        )
    return tuple(sorted(entries))


_SKILL_BYTECODE_AT_PYTEST_START = _skill_bytecode_snapshot()


@pytest.fixture(scope="session")
def skill_bytecode_at_pytest_start() -> tuple[tuple[str, int, int], ...]:
    return _SKILL_BYTECODE_AT_PYTEST_START
