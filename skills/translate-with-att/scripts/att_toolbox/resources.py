"""识别 RPG Maker 中不应作为玩家文本翻译的资源引用。"""

from __future__ import annotations

import re
from dataclasses import dataclass

_IMAGE_SUFFIXES = frozenset({".avif", ".bmp", ".gif", ".jpeg", ".jpg", ".png", ".svg", ".webp"})
_AUDIO_SUFFIXES = frozenset({".aac", ".flac", ".m4a", ".mid", ".midi", ".mp3", ".ogg", ".opus", ".wav"})
_VIDEO_SUFFIXES = frozenset({".avi", ".mov", ".mp4", ".ogv", ".webm"})
_FONT_SUFFIXES = frozenset({".eot", ".otf", ".ttf", ".woff", ".woff2"})
_ENCRYPTED_SUFFIXES = frozenset({".m4a_", ".ogg_", ".png_", ".rpgmvm", ".rpgmvo", ".rpgmvp"})
_ENCRYPTED_UNDERLYING_KIND = {
    ".m4a_": "audio",
    ".ogg_": "audio",
    ".png_": "image",
    ".rpgmvm": "video",
    ".rpgmvo": "audio",
    ".rpgmvp": "image",
}
_OTHER_RESOURCE_SUFFIXES = frozenset({".efkefc"})
_RESOURCE_SUFFIXES = {
    **{suffix: "image" for suffix in _IMAGE_SUFFIXES},
    **{suffix: "audio" for suffix in _AUDIO_SUFFIXES},
    **{suffix: "video" for suffix in _VIDEO_SUFFIXES},
    **{suffix: "font" for suffix in _FONT_SUFFIXES},
    **{suffix: "encrypted" for suffix in _ENCRYPTED_SUFFIXES},
    **{suffix: "other" for suffix in _OTHER_RESOURCE_SUFFIXES},
}
_RESOURCE_PREFIXES = {
    "audio/": "audio",
    "fonts/": "font",
    "img/": "image",
    "movies/": "video",
}
_IMAGE_FIELDS = frozenset(
    {
        "animation1Name",
        "animation2Name",
        "animationName",
        "battleback1Name",
        "battleback2Name",
        "battlerName",
        "characterName",
        "faceName",
        "parallaxName",
        "title1Name",
        "title2Name",
    }
)
_OTHER_RESOURCE_FIELDS = frozenset({"effectName"})
_FONT_FIELDS = frozenset({"mainFontFilename", "numberFontFilename"})
_AUDIO_CONTAINERS = frozenset(
    {"audio", "battleBgm", "bgm", "bgs", "defeatMe", "me", "se", "titleBgm", "victoryMe"}
)
_EVENT_IMAGE_PARAMETERS = frozenset(
    {
        (231, 1),  # Show Picture
        (283, 0),
        (283, 1),  # Change Battle Back
        (284, 0),  # Change Parallax
        (322, 1),
        (322, 3),
        (322, 5),  # Change Actor Images
        (323, 1),  # Change Vehicle Image
    }
)
_EVENT_VIDEO_PARAMETERS = frozenset({(261, 0)})
_EVENT_AUDIO_PARAMETERS = frozenset({(241, 0), (245, 0), (249, 0), (250, 0)})
_PLAIN_RESOURCE_NAME = re.compile(r"[^\s/\\]+\.[A-Za-z0-9_]+\Z")


@dataclass(frozen=True, slots=True)
class ResourceReference:
    """资源引用的机械分类依据。"""

    basis: str
    resource_kind: str


def _whole_value_kind(value: str) -> str | None:
    stripped = value.strip()
    if not stripped or stripped != value or any(character in value for character in ("\r", "\n", "\x00")):
        return None
    normalized = value.replace("\\", "/")
    lowered = normalized.casefold()
    suffix = next((candidate for candidate in _RESOURCE_SUFFIXES if lowered.endswith(candidate)), None)
    prefixed_kind = next(
        (
            resource_kind
            for prefix, resource_kind in _RESOURCE_PREFIXES.items()
            if lowered.startswith(prefix) and len(normalized) > len(prefix)
        ),
        None,
    )
    if suffix is not None and prefixed_kind is not None:
        return _RESOURCE_SUFFIXES[suffix]
    # 已知资源根可以包含带空格的文件名；其他路径与裸文件名必须不含空白，
    # 避免把“Please open path/to/Title.png”一类完整自然句误判为资源。
    if suffix is not None and (
        _PLAIN_RESOURCE_NAME.fullmatch(normalized) is not None
        or ("/" in normalized and not any(character.isspace() for character in normalized))
    ):
        return _RESOURCE_SUFFIXES[suffix]
    if prefixed_kind is not None:
        return prefixed_kind
    return None


def classify_resource_reference(
    path: tuple[str | int, ...],
    value: str,
    *,
    command_code: int | None = None,
    parameter: int | None = None,
) -> ResourceReference | None:
    """按标准字段、事件参数或完整资源路径识别一个字符串值。"""

    if not value.strip():
        return None
    last_key = path[-1] if path else None
    if isinstance(last_key, str):
        if last_key in _IMAGE_FIELDS:
            return ResourceReference("standard_resource_field", "image")
        if last_key in _FONT_FIELDS:
            return ResourceReference("standard_resource_field", "font")
        if last_key in _OTHER_RESOURCE_FIELDS:
            return ResourceReference("standard_resource_field", "other")
        if last_key == "name" and any(
            isinstance(step, str) and step in _AUDIO_CONTAINERS for step in path[:-1]
        ):
            return ResourceReference("standard_resource_field", "audio")
    if len(path) >= 2 and path[-2] == "tilesetNames" and isinstance(path[-1], int):
        return ResourceReference("standard_resource_field", "image")

    if command_code is not None and parameter is not None:
        command_parameter = (command_code, parameter)
        if command_parameter in _EVENT_IMAGE_PARAMETERS:
            return ResourceReference("event_resource_parameter", "image")
        if command_parameter in _EVENT_VIDEO_PARAMETERS:
            return ResourceReference("event_resource_parameter", "video")
        if command_parameter in _EVENT_AUDIO_PARAMETERS and last_key == "name":
            return ResourceReference("event_resource_parameter", "audio")

    resource_kind = _whole_value_kind(value)
    if resource_kind is not None:
        return ResourceReference("whole_resource_path", resource_kind)
    return None


def is_resource_file_suffix(suffix: str) -> bool:
    """判断文件后缀是否属于非文本资源；文本容器后缀不在此集合。"""

    return suffix.casefold() in _RESOURCE_SUFFIXES


def resource_kind_for_suffix(suffix: str) -> str | None:
    """返回一个资源后缀的稳定类别。"""

    normalized = suffix.casefold()
    return _ENCRYPTED_UNDERLYING_KIND.get(normalized, _RESOURCE_SUFFIXES.get(normalized))
