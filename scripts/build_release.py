"""构建 A.T.T MZ 发行版目录和 ZIP 包。

本脚本只负责发布包装，不保存源码数据库，不复制历史日志，也不把开发态
`skills/att-mz/SKILL.md` 放进发行包。发行包内的 `skills/att-mz/SKILL.md`
固定来自生成后的 `skills/att-mz-release/SKILL.md`，并按目标平台渲染命令入口。
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import subprocess
import sys
import zipfile
from dataclasses import dataclass
from io import TextIOWrapper
from pathlib import Path
from typing import cast


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT_DIR = ROOT / "dist"
RELEASE_DIRECTORY_NAME = "att-mz"
DEFAULT_TARGET_PLATFORM = "windows-x86_64"
RELEASE_SKILL_SOURCE = ROOT / "skills" / "att-mz-release" / "SKILL.md"
RELEASE_SKILL_REFERENCES_SOURCE = ROOT / "skills" / "att-mz-release" / "references"
RELEASE_README_SOURCE = ROOT / "README.md"
RELEASE_EXECUTABLE_PLACEHOLDER = "<A.T.T MZ 可执行文件>"


@dataclass(frozen=True)
class PlatformSpec:
    """发行目标平台的包名、入口和 Skill 命令渲染规则。"""

    target_platform: str
    zip_name: str
    executable_name: str
    command_prefix: str


PLATFORM_SPECS: dict[str, PlatformSpec] = {
    "windows-x86_64": PlatformSpec(
        target_platform="windows-x86_64",
        zip_name="att-mz-windows-x86_64.zip",
        executable_name="att-mz.exe",
        command_prefix=".\\att-mz.exe",
    ),
    "linux-x86_64": PlatformSpec(
        target_platform="linux-x86_64",
        zip_name="att-mz-linux-x86_64.zip",
        executable_name="att-mz",
        command_prefix="./att-mz",
    ),
}
EXECUTABLE_FILE_NAMES = frozenset(spec.executable_name for spec in PLATFORM_SPECS.values())
EXPECTED_RUNNER_SYSTEMS = {
    "windows-x86_64": "Windows",
    "linux-x86_64": "Linux",
}
X86_64_MACHINE_NAMES = {"AMD64", "x86_64"}


@dataclass(frozen=True)
class BuildOptions:
    """发布构建参数。"""

    output_dir: Path
    zip_name: str
    platform_spec: PlatformSpec


@dataclass(frozen=True)
class CopySpec:
    """发行包资源复制规则。"""

    source: Path
    target_parts: tuple[str, ...]


def parse_args() -> BuildOptions:
    """解析命令行参数。"""
    parser = argparse.ArgumentParser(description="构建 A.T.T MZ 发行版 ZIP")
    _ = parser.add_argument(
        "--target-platform",
        choices=tuple(PLATFORM_SPECS),
        default=DEFAULT_TARGET_PLATFORM,
        help=f"目标平台，默认 {DEFAULT_TARGET_PLATFORM}",
    )
    _ = parser.add_argument(
        "--output-dir",
        default=str(DEFAULT_OUTPUT_DIR),
        help="发行目录输出位置，默认写入 dist",
    )
    _ = parser.add_argument(
        "--zip-name",
        default=None,
        help="生成的 ZIP 文件名；不传时按目标平台自动选择",
    )
    namespace = parser.parse_args()
    target_platform = cast(str, namespace.target_platform)
    platform_spec = PLATFORM_SPECS[target_platform]
    output_dir = cast(str, namespace.output_dir)
    raw_zip_name = cast(str | None, namespace.zip_name)
    return BuildOptions(
        output_dir=Path(output_dir).resolve(),
        zip_name=raw_zip_name or platform_spec.zip_name,
        platform_spec=platform_spec,
    )


def ensure_source_exists(path: Path) -> None:
    """确认发布资源存在。"""
    if not path.exists():
        raise FileNotFoundError(f"发布资源不存在: {path}")


def configure_stdio_encoding() -> None:
    """把发布脚本输出固定为 UTF-8，避免 GitHub Windows runner 使用窄编码。"""
    for stream in (sys.stdout, sys.stderr):
        if isinstance(stream, TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")


def ensure_github_actions_environment() -> None:
    """保证发行版只能由 GitHub Actions 构建。"""
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError("发行版构建只能在 GitHub Actions release 工作流中执行。")


def ensure_target_platform_matches_runner(platform_spec: PlatformSpec) -> None:
    """确认当前 runner 与目标发行平台一致。"""
    expected_system = EXPECTED_RUNNER_SYSTEMS[platform_spec.target_platform]
    actual_system = platform.system()
    actual_machine = platform.machine()
    if actual_system != expected_system or actual_machine not in X86_64_MACHINE_NAMES:
        message = f"目标发行平台与当前 runner 不一致: target={platform_spec.target_platform}, runner={actual_system}/{actual_machine}"
        raise RuntimeError(message)


def reset_release_directory(release_dir: Path) -> None:
    """清空并重建发行目录。"""
    if release_dir.exists():
        shutil.rmtree(release_dir)
    release_dir.mkdir(parents=True)


def release_subprocess_env() -> dict[str, str]:
    """返回发布构建子进程的稳定环境。"""
    env = os.environ.copy()
    env.setdefault("PYTHONUTF8", "1")
    env.setdefault("PYTHONIOENCODING", "utf-8")
    return env


def build_release_entrypoint(release_dir: Path, platform_spec: PlatformSpec) -> None:
    """按平台构建发行包入口。"""
    executable_path = release_dir / platform_spec.executable_name
    build_pex_scie(executable_path)


def build_pex_scie(executable_path: Path) -> None:
    """使用 PEX scie eager 构建当前 runner 平台的可执行文件。

    --venv-copies 让 pex 用文件复制代替符号链接安装依赖，避免 Windows 普通账户
    因缺少创建符号链接特权（WinError 1314）导致 exe 启动失败。--scie-pbs-stripped
    去掉 scie 自带的 pbs 引导副本，保持单体 exe 体积精简。
    """
    pex_output_path = executable_path.with_suffix(".pex")
    if pex_output_path.exists():
        pex_output_path.unlink()
    if executable_path.exists():
        executable_path.unlink()
    command = [
        "uv",
        "run",
        "--with",
        "pex",
        "pex",
        ".",
        "--script",
        "att-mz",
        "--venv",
        "--venv-copies",
        "--venv-site-packages-copies",
        "--scie",
        "eager",
        "--scie-pbs-stripped",
        "--scie-load-dotenv",
        "--output-file",
        str(pex_output_path),
    ]
    _ = subprocess.run(command, cwd=ROOT, check=True, env=release_subprocess_env())
    ensure_source_exists(executable_path)
    executable_path.chmod(executable_path.stat().st_mode | 0o755)
    if pex_output_path.exists():
        pex_output_path.unlink()


def copy_file(source: Path, target: Path) -> None:
    """复制单个文件并确保目标目录存在。"""
    ensure_source_exists(source)
    target.parent.mkdir(parents=True, exist_ok=True)
    _ = shutil.copy2(source, target)


def render_release_text_for_platform(text: str, platform_spec: PlatformSpec) -> str:
    """把发行版 Skill 中的平台占位入口替换为当前包的真实命令。"""
    return text.replace(RELEASE_EXECUTABLE_PLACEHOLDER, platform_spec.command_prefix)


def copy_text_file_for_platform(source: Path, target: Path, platform_spec: PlatformSpec) -> None:
    """复制文本资源，并渲染发行包内的平台命令入口。"""
    ensure_source_exists(source)
    text = source.read_text(encoding="utf-8")
    rendered_text = render_release_text_for_platform(text, platform_spec)
    target.parent.mkdir(parents=True, exist_ok=True)
    _ = target.write_text(rendered_text, encoding="utf-8")


def copy_packaged_release_skill(target: Path, platform_spec: PlatformSpec) -> None:
    """把发行版 Skill 模板写成发行包内的 `att-mz` Skill。"""
    ensure_source_exists(RELEASE_SKILL_SOURCE)
    skill_text = RELEASE_SKILL_SOURCE.read_text(encoding="utf-8")
    packaged_skill_text = skill_text.replace("name: att-mz-release", "name: att-mz", 1)
    packaged_skill_text = render_release_text_for_platform(packaged_skill_text, platform_spec)
    target.parent.mkdir(parents=True, exist_ok=True)
    _ = target.write_text(packaged_skill_text, encoding="utf-8")


def copy_release_resources(release_dir: Path, platform_spec: PlatformSpec) -> None:
    """复制发行包所需的配置、文档、字体、提示词和 Skill。"""
    copy_specs = [
        CopySpec(RELEASE_README_SOURCE, ("README.md",)),
        CopySpec(ROOT / "LICENSE", ("LICENSE",)),
        CopySpec(ROOT / "setting.example.toml", ("setting.example.toml",)),
        CopySpec(ROOT / "setting.example.toml", ("setting.toml",)),
        CopySpec(ROOT / "custom_placeholder_rules.json", ("custom_placeholder_rules.json",)),
        CopySpec(ROOT / "prompts" / "text_translation_ja_to_zh_system.md", ("prompts", "text_translation_ja_to_zh_system.md")),
        CopySpec(ROOT / "prompts" / "text_translation_en_to_zh_system.md", ("prompts", "text_translation_en_to_zh_system.md")),
        CopySpec(ROOT / "fonts" / "NotoSansSC-Regular.ttf", ("fonts", "NotoSansSC-Regular.ttf")),
    ]
    for spec in copy_specs:
        copy_file(spec.source, release_dir.joinpath(*spec.target_parts))
    for reference_path in sorted(RELEASE_SKILL_REFERENCES_SOURCE.glob("*.md")):
        copy_text_file_for_platform(
            reference_path,
            release_dir / "skills" / "att-mz" / "references" / reference_path.name,
            platform_spec,
        )
    copy_packaged_release_skill(release_dir / "skills" / "att-mz" / "SKILL.md", platform_spec)

    for directory_parts in (("data", "db"), ("logs",), ("outputs",)):
        release_dir.joinpath(*directory_parts).mkdir(parents=True, exist_ok=True)


def run_smoke_tests(release_dir: Path, platform_spec: PlatformSpec) -> None:
    """验证发行版入口能启动并能读取空注册表。"""
    exe_path = release_dir / platform_spec.executable_name
    _ = subprocess.run(
        [str(exe_path), "--help"],
        cwd=release_dir,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    _ = subprocess.run(
        [str(exe_path), "list"],
        cwd=release_dir,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def add_directory_entry(archive: zipfile.ZipFile, arcname: str) -> None:
    """向 ZIP 写入空目录条目。"""
    normalized_name = arcname.replace("\\", "/").rstrip("/") + "/"
    info = zipfile.ZipInfo(normalized_name)
    info.date_time = (2026, 1, 1, 0, 0, 0)
    info.external_attr = 0o755 << 16
    archive.writestr(info, b"")


def add_file_entry(archive: zipfile.ZipFile, source: Path, arcname: str) -> None:
    """向 ZIP 写入单个文件。"""
    info = zipfile.ZipInfo(arcname.replace("\\", "/"))
    info.date_time = (2026, 1, 1, 0, 0, 0)
    info.compress_type = zipfile.ZIP_DEFLATED
    file_mode = 0o755 if source.name in EXECUTABLE_FILE_NAMES else 0o644
    info.external_attr = file_mode << 16
    archive.writestr(info, source.read_bytes())


def create_release_zip(release_dir: Path, zip_path: Path) -> None:
    """把发行目录压缩为 ZIP。"""
    if zip_path.exists():
        zip_path.unlink()
    with zipfile.ZipFile(zip_path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        root_arcname = release_dir.name
        add_directory_entry(archive, root_arcname)
        for directory in sorted(path for path in release_dir.rglob("*") if path.is_dir()):
            add_directory_entry(archive, str(Path(root_arcname) / directory.relative_to(release_dir)))
        for file_path in sorted(path for path in release_dir.rglob("*") if path.is_file()):
            add_file_entry(archive, file_path, str(Path(root_arcname) / file_path.relative_to(release_dir)))


def main() -> int:
    """执行发行版构建。"""
    configure_stdio_encoding()
    ensure_github_actions_environment()
    options = parse_args()
    ensure_target_platform_matches_runner(options.platform_spec)
    release_dir = options.output_dir / RELEASE_DIRECTORY_NAME
    zip_path = options.output_dir / options.zip_name

    reset_release_directory(release_dir)
    build_release_entrypoint(release_dir, options.platform_spec)

    copy_release_resources(release_dir, options.platform_spec)
    run_smoke_tests(release_dir, options.platform_spec)
    create_release_zip(release_dir, zip_path)
    print(f"目标平台: {options.platform_spec.target_platform}")
    print(f"发行版目录: {release_dir}")
    print(f"发行版 ZIP: {zip_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
