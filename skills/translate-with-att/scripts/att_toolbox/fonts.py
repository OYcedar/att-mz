"""RPG Maker 字体计划的稳定公共入口。"""

from att_toolbox.font_metadata import font_codepoints
from att_toolbox.font_references import FontPlan, build_font_plan
from att_toolbox.font_transaction import (
    FontGameLock,
    FontGameLockRelease,
    FontStateBinding,
    FontStateIntegrityError,
    acquire_font_game_lock,
    apply_font_plan,
    bind_font_state,
    font_game_lock_paths,
    font_state_files,
    release_font_game_lock,
    restore_font_state,
    verify_applied_font_plan,
    verify_font_plan_source,
    verify_restored_font_state,
    write_font_apply_marker,
)

__all__ = [
    "FontGameLock",
    "FontGameLockRelease",
    "FontPlan",
    "FontStateBinding",
    "FontStateIntegrityError",
    "acquire_font_game_lock",
    "apply_font_plan",
    "bind_font_state",
    "build_font_plan",
    "font_codepoints",
    "font_game_lock_paths",
    "font_state_files",
    "release_font_game_lock",
    "restore_font_state",
    "verify_applied_font_plan",
    "verify_font_plan_source",
    "verify_restored_font_state",
    "write_font_apply_marker",
]
