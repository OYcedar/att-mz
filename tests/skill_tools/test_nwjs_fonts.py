from __future__ import annotations

import base64
import hashlib
import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time
from collections.abc import Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

import pytest
from att_skill_tools import ToolError, atomic_write_directory
from att_toolbox import font_references, font_transaction
from att_toolbox.font_transaction import ByteMutation
from att_toolbox.fonts import (
    FontPlan,
    apply_font_plan,
    build_font_plan,
    font_codepoints,
    font_state_files,
    restore_font_state,
)
from att_toolbox.nwjs import (
    OBSERVER_SCRIPT,
    CdpConnection,
    CdpEvaluationError,
    CdpProtocolError,
    CdpTarget,
    CdpUnavailableError,
    build_nwjs_command,
    loopback_listener_pids,
    owned_loopback_listener_pid,
    process_tree_pids,
    reserve_loopback_port,
    scenario_expression,
    summarize_draws,
    unique_content_target,
)
from inspect_nwjs_runtime import scenario_action

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "skills" / "translate-with-att" / "scripts"
FONT_ROOT = ROOT / "skills" / "translate-with-att" / "assets" / "fonts"
SOURCE_MANIFEST = ROOT / "licenses" / "fonts" / "SOURCES.json"


@dataclass(frozen=True, slots=True)
class _TransactionPlan:
    game_root: Path
    selected_font: Path
    selected_sha256: str
    mutations: tuple[ByteMutation, ...]


def _run_script(
    script: Path,
    arguments: Sequence[object],
    *,
    expected: int = 0,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [sys.executable, str(script), *(str(argument) for argument in arguments)],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    assert result.returncode == expected, f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    return result


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")


@pytest.fixture
def font_game(tmp_path: Path) -> Path:
    game = tmp_path / "isolated-game"
    (game / "data").mkdir(parents=True)
    (game / "fonts").mkdir()
    (game / "js" / "plugins").mkdir(parents=True)
    (game / "Game.exe").write_bytes(b"")
    (game / "package.json").write_text('{"main":"index.html"}', encoding="utf-8")
    (game / "js" / "rmmz_core.js").write_text("// MZ", encoding="utf-8")
    (game / "fonts" / "OldBody.ttf").write_bytes(b"old-body-font")
    (game / "fonts" / "IconFont.woff").write_bytes(b"old-icon-font")
    (game / "fonts" / "gamefont.css").write_text(
        "@font-face { font-family: 'GameFont'; src: url('OldBody.ttf') /* keep */ format('woff2'); }\n"
        "@font-face { font-family: IconFamily; src: url(IconFont.woff); }\n"
        "body { font-family: GameFont, 'IconFamily', sans-serif; }\n",
        encoding="utf-8",
    )
    (game / "index.html").write_text(
        "<!doctype html>\n"
        "<!-- <style>@font-face {font-family: Fake; src:url('OldBody.ttf')}</style> -->\n"
        "<p title=GameFont>GameFont; font-family: GameFont; fonts/OldBody.ttf</p>\n"
        "<style>@font-face { font-family: 'HtmlFont'; src: url('fonts/OldBody.ttf') "
        "format('woff2'); } .task { font-family: GameFont; }</style>\n"
        '<div style="font-family: GameFont" title="GameFont"></div>\n'
        "<link as=font href=fonts/OldBody.ttf />\n"
        "<script>const htmlPath = 'fonts\\/OldBody.ttf'; "
        "const htmlFont = {fontFace:'GameFont', caption:'GameFont'};</script>\n",
        encoding="utf-8",
    )
    _write_json(
        game / "data" / "System.json",
        {
            "advanced": {"mainFontFilename": "OldBody.ttf", "numberFontFilename": "IconFont.woff"},
            "ordinaryText": "GameFont",
            "ordinaryStem": "OldBody",
        },
    )
    (game / "data" / "Raw.json").write_text(
        '{  "fontFamily" : "GameFont", "fontFamily":"GameFont", '
        '"payload":"{  \\"fontFamily\\" : \\"GameFont\\", '
        '\\"fontFamily\\":\\"GameFont\\", '
        '\\"fontFile\\":\\"fonts\\\\/OldBody.ttf\\", '
        '\\"caption\\":\\"GameFont\\" }", "caption":"GameFont" }\n',
        encoding="utf-8",
    )
    plugins: list[dict[str, object]] = [
        {
            "name": "FontConsumer",
            "status": True,
            "description": "",
            "parameters": {
                "fontFamily": "GameFont",
                "caption": "GameFont",
                "Nested": '{"fontFamily":"GameFont","caption":"GameFont"}',
            },
        },
        {
            "name": "InactiveFont",
            "status": False,
            "description": "",
            "parameters": {},
        },
    ]
    (game / "js" / "plugins.js").write_text(
        f"var $plugins = {json.dumps(plugins, ensure_ascii=False)};\n",
        encoding="utf-8",
    )
    (game / "js" / "plugins" / "FontConsumer.js").write_text(
        "Graphics.loadFont('GameFont', 'fonts/OldBody.ttf');\n"
        "const styled = {fontFace: 'GameFont', caption: 'GameFont'};\n"
        "const directPath = 'fonts\\/OldBody.ttf';\n"
        "const ordinaryStem = 'OldBody';\n"
        'const nested = \'{  "fontFamily" : "GameFont", "fontFamily":"GameFont", '
        '"fontFile":"fonts\\/OldBody.ttf", "caption":"GameFont" }\';\n',
        encoding="utf-8",
    )
    (game / "js" / "plugins" / "InactiveFont.js").write_text(
        "Graphics.loadFont('GameFont', 'fonts/OldBody.ttf');\n"
        "const styled = {fontFace: 'GameFont'};\n"
        "const directPath = 'fonts/OldBody.ttf';\n",
        encoding="utf-8",
    )
    (game / "settings.ini").write_text(
        "font_family=GameFont\ncaption=GameFont\nfont_file=fonts/OldBody.ttf\n",
        encoding="utf-8",
    )
    return game


def _mutation_text(plan: FontPlan, relative: str) -> str:
    for mutation in plan.mutations:
        if mutation.relative_path == relative:
            return mutation.replacement.decode("utf-8")
    raise AssertionError(f"missing mutation: {relative}")


def test_font_graph_rewrites_proven_contexts_but_not_matching_body_text(font_game: Path) -> None:
    selected = FONT_ROOT / "NotoSansCJKsc-Regular.otf"
    plan = build_font_plan(
        game_root=font_game.resolve(),
        content_root=font_game.resolve(),
        selected_font=selected,
    )
    aliases = {(item.value, item.asset, item.basis) for item in plan.aliases}
    assert ("GameFont", "fonts/OldBody.ttf", "css_font_face") in aliases
    assert any(item.value == "IconFamily" and item.asset == "fonts/IconFont.woff" for item in plan.aliases)
    assert {reference.context for reference in plan.references} >= {
        "css_url_asset_path",
        "css_font_family_font_alias",
        "json_asset_path",
        "javascript_font_alias",
        "config_complete_value_font_alias",
    }

    system = json.loads(_mutation_text(plan, "data/System.json"))
    assert system["advanced"]["mainFontFilename"] == selected.name
    assert system["advanced"]["numberFontFilename"] == selected.name
    assert system["ordinaryText"] == "GameFont"
    assert system["ordinaryStem"] == "OldBody"

    plugin_config = _mutation_text(plan, "js/plugins.js")
    assert '"fontFamily": "NotoSansCJKsc-Regular"' in plugin_config
    assert '"caption": "GameFont"' in plugin_config
    active_plugin = _mutation_text(plan, "js/plugins/FontConsumer.js")
    assert "fontFace: 'NotoSansCJKsc-Regular'" in active_plugin
    assert "caption: 'GameFont'" in active_plugin
    assert "ordinaryStem = 'OldBody'" in active_plugin
    assert f"directPath = 'fonts\\/{selected.name}'" in active_plugin
    assert active_plugin.count('"fontFamily":"NotoSansCJKsc-Regular"') == 1
    assert '"fontFamily" : "NotoSansCJKsc-Regular"' in active_plugin
    assert f'"fontFile":"fonts\\/{selected.name}"' in active_plugin
    assert '"caption":"GameFont"' in active_plugin
    assert not any(mutation.relative_path == "js/plugins/InactiveFont.js" for mutation in plan.mutations)
    assert "fontFace: 'GameFont'" in (font_game / "js" / "plugins" / "InactiveFont.js").read_text(
        encoding="utf-8"
    )
    assert any(
        item.source == "js/plugins/InactiveFont.js"
        and item.reason == "inactive_or_unproven_javascript_font_consumer"
        for item in plan.reviews
    )

    settings = _mutation_text(plan, "settings.ini")
    assert "font_family=NotoSansCJKsc-Regular" in settings
    assert "caption=GameFont" in settings
    assert f"font_file=fonts/{selected.name}" in settings
    raw_json = _mutation_text(plan, "data/Raw.json")
    assert raw_json.count('"fontFamily":"NotoSansCJKsc-Regular"') == 1
    assert raw_json.count('"fontFamily" : "NotoSansCJKsc-Regular"') == 1
    assert raw_json.count('\\"fontFamily\\":\\"NotoSansCJKsc-Regular\\"') == 1
    assert raw_json.count('\\"fontFamily\\" : \\"NotoSansCJKsc-Regular\\"') == 1
    assert f'\\"fontFile\\":\\"fonts\\\\/{selected.name}\\"' in raw_json
    assert '"caption":"GameFont"' in raw_json
    assert '\\"caption\\":\\"GameFont\\"' in raw_json

    css = _mutation_text(plan, "fonts/gamefont.css")
    assert "/* keep */" in css
    assert "format('opentype')" in css
    html = _mutation_text(plan, "index.html")
    assert "<p title=GameFont>GameFont; font-family: GameFont; fonts/OldBody.ttf</p>" in html
    assert "<!-- <style>@font-face {font-family: Fake; src:url('OldBody.ttf')}</style> -->" in html
    assert f"url('fonts/{selected.name}') format('opentype')" in html
    assert 'style="font-family: NotoSansCJKsc-Regular"' in html
    assert 'title="GameFont"' in html
    assert f"href=fonts/{selected.name}" in html
    assert f"htmlPath = 'fonts\\/{selected.name}'" in html
    assert "fontFace:'NotoSansCJKsc-Regular'" in html
    assert "caption:'GameFont'" in html
    assert any(item.reason == "unresolved_json_font_value" for item in plan.reviews)
    assert any(item.reason == "unresolved_javascript_font_value" for item in plan.reviews)
    assert any(item.reason == "unclassified_or_partial_font_context" for item in plan.reviews)


def test_font_plan_enumerates_game_tree_once(font_game: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    original = font_references.safe_walk_files
    calls = 0

    def counted(root: Path) -> Iterator[Path]:
        nonlocal calls
        calls += 1
        return original(root)

    monkeypatch.setattr(font_references, "safe_walk_files", counted)
    font_references.build_font_plan(
        game_root=font_game.resolve(),
        content_root=font_game.resolve(),
        selected_font=FONT_ROOT / "NotoSansCJKsc-Regular.otf",
    )

    assert calls == 1


def test_css_font_format_tracks_selected_ttf(font_game: Path) -> None:
    selected = FONT_ROOT / "LXGWWenKaiGB-Regular.ttf"
    plan = build_font_plan(
        game_root=font_game.resolve(),
        content_root=font_game.resolve(),
        selected_font=selected,
    )
    css = _mutation_text(plan, "fonts/gamefont.css")
    html = _mutation_text(plan, "index.html")
    assert "format('truetype')" in css
    assert f"url('fonts/{selected.name}') format('truetype')" in html


def test_font_apply_restore_and_restore_drift(font_game: Path, tmp_path: Path) -> None:
    selected = FONT_ROOT / "NotoSansCJKsc-Regular.otf"
    plan = build_font_plan(
        game_root=font_game.resolve(),
        content_root=font_game.resolve(),
        selected_font=selected,
    )
    originals = {
        mutation.relative_path: mutation.original
        for mutation in plan.mutations
        if mutation.original is not None
    }
    state = tmp_path / "font-state"
    atomic_write_directory(state, font_state_files(plan), replace=False)
    apply_font_plan(plan, state=state)
    assert json.loads((state / "status.json").read_text(encoding="utf-8"))["status"] == "applied"
    created = font_game / "fonts" / selected.name
    assert created.read_bytes() == selected.read_bytes()

    css = font_game / "fonts" / "gamefont.css"
    expected_after = css.read_bytes()
    css.write_bytes(expected_after + b"\n/* drift */")
    with pytest.raises(ToolError, match="既不等于"):
        restore_font_state(game_root=font_game.resolve(), state=state)
    css.write_bytes(expected_after)

    assert restore_font_state(game_root=font_game.resolve(), state=state) == len(plan.mutations)
    assert json.loads((state / "status.json").read_text(encoding="utf-8"))["status"] == "restored"
    assert not created.exists()
    for relative, original in originals.items():
        assert (font_game / Path(relative)).read_bytes() == original


def test_apply_failure_does_not_touch_unattempted_concurrent_change(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    game = (tmp_path / "transaction-game").resolve()
    game.mkdir()
    selected = tmp_path / "selected.ttf"
    selected.write_bytes(b"selected")
    paths = [game / name for name in ("first.txt", "second.txt", "third.txt")]
    for index, path in enumerate(paths, start=1):
        path.write_bytes(f"before-{index}".encode())
    mutations = tuple(
        ByteMutation(path.name, path.read_bytes(), f"after-{index}".encode())
        for index, path in enumerate(paths, start=1)
    )
    plan = _TransactionPlan(game, selected, hashlib.sha256(b"selected").hexdigest(), mutations)
    state = tmp_path / "apply-state"
    atomic_write_directory(state, font_transaction.font_state_files(plan), replace=False)
    original_replace = os.replace
    calls = 0

    def failing_replace(source: str | bytes | Path, target: str | bytes | Path) -> None:
        nonlocal calls
        calls += 1
        if calls == 2:
            paths[2].write_bytes(b"external-third")
            raise OSError("injected second write failure")
        original_replace(source, target)

    monkeypatch.setattr(os, "replace", failing_replace)
    with pytest.raises(ToolError, match="写入失败"):
        font_transaction.apply_font_plan(plan, state=state)
    assert paths[0].read_bytes() == b"before-1"
    assert paths[1].read_bytes() == b"before-2"
    assert paths[2].read_bytes() == b"external-third"
    assert json.loads((state / "status.json").read_text(encoding="utf-8"))["status"] == "rolled_back"


def test_apply_rollback_failure_marks_recovery_required(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    game = (tmp_path / "recovery-required-game").resolve()
    game.mkdir()
    selected = tmp_path / "selected.ttf"
    selected.write_bytes(b"selected")
    first = game / "first.txt"
    second = game / "second.txt"
    first.write_bytes(b"before-first")
    second.write_bytes(b"before-second")
    plan = _TransactionPlan(
        game,
        selected,
        hashlib.sha256(b"selected").hexdigest(),
        (
            ByteMutation(first.name, first.read_bytes(), b"after-first"),
            ByteMutation(second.name, second.read_bytes(), b"after-second"),
        ),
    )
    state = tmp_path / "recovery-required-state"
    atomic_write_directory(state, font_transaction.font_state_files(plan), replace=False)
    original_replace = os.replace
    second_failed = False

    def failing_replace(source: str | bytes | Path, target: str | bytes | Path) -> None:
        nonlocal second_failed
        target_path = Path(os.fsdecode(target))
        if target_path == second and not second_failed:
            second_failed = True
            raise OSError("injected apply failure")
        if target_path == first and second_failed:
            raise OSError("injected rollback failure")
        original_replace(source, target)

    monkeypatch.setattr(os, "replace", failing_replace)
    with pytest.raises(ToolError, match="回滚无法确认"):
        font_transaction.apply_font_plan(plan, state=state)
    assert first.read_bytes() == b"after-first"
    assert second.read_bytes() == b"before-second"
    status = json.loads((state / "status.json").read_text(encoding="utf-8"))
    assert status == {"status": "recovery_required"}


def test_restore_accepts_interrupted_mix_of_before_and_after(tmp_path: Path) -> None:
    game = (tmp_path / "interrupted-game").resolve()
    game.mkdir()
    selected = tmp_path / "selected.ttf"
    selected.write_bytes(b"selected")
    already_before = game / "already-before.txt"
    needs_restore = game / "needs-restore.txt"
    created = game / "created.ttf"
    already_before.write_bytes(b"before-one")
    needs_restore.write_bytes(b"after-two")
    created.write_bytes(b"created-after")
    plan = _TransactionPlan(
        game,
        selected,
        hashlib.sha256(b"selected").hexdigest(),
        (
            ByteMutation(already_before.name, b"before-one", b"after-one"),
            ByteMutation(needs_restore.name, b"before-two", b"after-two"),
            ByteMutation(created.name, None, b"created-after"),
        ),
    )
    state = tmp_path / "interrupted-state"
    atomic_write_directory(state, font_transaction.font_state_files(plan), replace=False)

    assert font_transaction.restore_font_state(game_root=game, state=state) == 2
    assert already_before.read_bytes() == b"before-one"
    assert needs_restore.read_bytes() == b"before-two"
    assert not created.exists()
    assert json.loads((state / "status.json").read_text(encoding="utf-8")) == {"status": "restored"}


def test_restore_failure_rolls_forward_only_attempted_entries(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    game = (tmp_path / "restore-game").resolve()
    game.mkdir()
    selected = tmp_path / "selected.ttf"
    selected.write_bytes(b"selected")
    paths = [game / name for name in ("first.txt", "second.txt", "third.txt")]
    mutations: list[ByteMutation] = []
    for index, path in enumerate(paths, start=1):
        before = f"before-{index}".encode()
        after = f"after-{index}".encode()
        path.write_bytes(after)
        mutations.append(ByteMutation(path.name, before, after))
    plan = _TransactionPlan(
        game,
        selected,
        hashlib.sha256(b"selected").hexdigest(),
        tuple(mutations),
    )
    state = tmp_path / "restore-state"
    atomic_write_directory(state, font_transaction.font_state_files(plan), replace=False)
    original_replace = os.replace
    calls = 0

    def failing_replace(source: str | bytes | Path, target: str | bytes | Path) -> None:
        nonlocal calls
        calls += 1
        if calls == 2:
            paths[0].write_bytes(b"external-first")
            raise OSError("injected second restore failure")
        original_replace(source, target)

    monkeypatch.setattr(os, "replace", failing_replace)
    with pytest.raises(ToolError, match="restore 写入失败"):
        font_transaction.restore_font_state(game_root=game, state=state)
    assert paths[0].read_bytes() == b"external-first"
    assert paths[1].read_bytes() == b"after-2"
    assert paths[2].read_bytes() == b"after-3"


def test_font_cli_named_bundle_noop_is_success(tmp_path: Path) -> None:
    game = tmp_path / "no-font-references"
    (game / "data").mkdir(parents=True)
    (game / "js").mkdir()
    (game / "Game.exe").write_bytes(b"")
    (game / "package.json").write_text("{}", encoding="utf-8")
    (game / "js" / "rmmz_core.js").write_text("// MZ", encoding="utf-8")
    (game / "js" / "plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
    _write_json(game / "data" / "System.json", {})
    output = tmp_path / "noop.json"
    state = tmp_path / "unused-state"
    _run_script(
        SCRIPTS / "manage_rpg_maker_fonts.py",
        [
            "apply",
            "--game",
            game,
            "--font",
            "noto-sans-sc",
            "--state",
            state,
            "--output",
            output,
        ],
    )
    report = json.loads(output.read_text(encoding="utf-8"))
    assert report["no_op"] is True
    assert report["applied"] is False
    assert report["qa_status"] == "unverified"
    assert not state.exists()


def test_font_cli_already_selected_reference_is_clean(tmp_path: Path) -> None:
    selected = FONT_ROOT / "NotoSansCJKsc-Regular.otf"
    game = tmp_path / "selected-font-game"
    (game / "data").mkdir(parents=True)
    (game / "fonts").mkdir()
    (game / "js").mkdir()
    (game / "Game.exe").write_bytes(b"")
    (game / "package.json").write_text("{}", encoding="utf-8")
    (game / "js" / "rmmz_core.js").write_text("// MZ", encoding="utf-8")
    (game / "js" / "plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
    (game / "fonts" / selected.name).write_bytes(selected.read_bytes())
    _write_json(
        game / "data" / "System.json",
        {"advanced": {"mainFontFilename": selected.name}},
    )
    output = tmp_path / "selected-font.json"
    _run_script(
        SCRIPTS / "manage_rpg_maker_fonts.py",
        ["inspect", "--game", game, "--font", "noto-sans-sc", "--output", output],
    )
    report = json.loads(output.read_text(encoding="utf-8"))
    assert report["confirmed_reference_count"] == 1
    assert report["mutation_count"] == 0
    assert report["qa_status"] == "clean"


def test_bundled_fonts_match_official_manifest_and_cover_common_chinese() -> None:
    manifest = json.loads(SOURCE_MANIFEST.read_text(encoding="utf-8"))
    assert {item["alias"] for item in manifest["fonts"]} == {
        "noto-sans-sc",
        "noto-serif-sc",
        "lxgw-wenkai",
    }
    for item in manifest["fonts"]:
        path = ROOT / item["file"]
        body = path.read_bytes()
        assert len(body) == item["size"]
        assert hashlib.sha256(body).hexdigest() == item["sha256"]
        codepoints = font_codepoints(path)
        assert all(ord(character) in codepoints for character in "中文汉化，。！？ABC123")
    lxgw = next(item for item in manifest["fonts"] if item["alias"] == "lxgw-wenkai")
    assert lxgw["internal_version"] == "Version 1.522; March 17, 2026"


def _server_frame(payload: bytes, *, opcode: int, final: bool) -> bytes:
    first = (0x80 if final else 0) | opcode
    if len(payload) < 126:
        return bytes((first, len(payload))) + payload
    return bytes((first, 126)) + struct.pack("!H", len(payload)) + payload


def _read_client_frame(connection: socket.socket) -> tuple[int, bytes]:
    first, second = connection.recv(2)
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", connection.recv(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", connection.recv(8))[0]
    mask = connection.recv(4)
    payload = bytearray()
    while len(payload) < length:
        payload.extend(connection.recv(length - len(payload)))
    return first & 0x0F, bytes(value ^ mask[index % 4] for index, value in enumerate(payload))


def _listener_child() -> tuple[subprocess.Popen[bytes], int, int]:
    port = reserve_loopback_port()
    code = (
        "import socket,time;"
        "listener=socket.socket();"
        f"listener.bind(('127.0.0.1',{port}));"
        "listener.listen();"
        "time.sleep(30)"
    )
    process = subprocess.Popen(
        [sys.executable, "-c", code],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError("listener child exited before binding")
        listeners = loopback_listener_pids(port)
        matching = listeners & process_tree_pids(process.pid)
        if len(matching) == 1:
            return process, port, next(iter(matching))
        time.sleep(0.02)
    process.terminate()
    process.wait(timeout=5)
    raise AssertionError("listener child did not bind in time")


@pytest.mark.skipif(os.name != "nt", reason="Windows TCP owner and Toolhelp32 contract")
def test_listener_identity_accepts_owned_root_pid() -> None:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        port = cast(int, listener.getsockname()[1])
        assert owned_loopback_listener_pid(port, os.getpid()) == os.getpid()


@pytest.mark.skipif(os.name != "nt", reason="Windows TCP owner and Toolhelp32 contract")
def test_listener_identity_accepts_owned_child_pid() -> None:
    process, port, listener_pid = _listener_child()
    try:
        assert listener_pid != process.pid
        assert owned_loopback_listener_pid(port, process.pid) == listener_pid
    finally:
        process.terminate()
        process.wait(timeout=5)


@pytest.mark.skipif(os.name != "nt", reason="Windows TCP owner and Toolhelp32 contract")
def test_listener_identity_rejects_unrelated_pid_that_takes_port() -> None:
    listener, port, _listener_pid = _listener_child()
    unrelated_root = subprocess.Popen(
        [sys.executable, "-c", "import time;time.sleep(30)"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        with pytest.raises(CdpUnavailableError, match="无关进程"):
            owned_loopback_listener_pid(port, unrelated_root.pid)
    finally:
        listener.terminate()
        listener.wait(timeout=5)
        unrelated_root.terminate()
        unrelated_root.wait(timeout=5)


def test_page_target_requires_unique_natural_game_entry(tmp_path: Path) -> None:
    content = tmp_path / "www"
    content.mkdir()
    entry = content / "index.html"
    entry.write_text("<!doctype html>", encoding="utf-8")
    expected = CdpTarget("game", entry.as_uri(), "ws://127.0.0.1:9000/devtools/page/game")
    other_entry = tmp_path / "other.html"
    other_entry.write_text("<!doctype html>", encoding="utf-8")
    other = CdpTarget("other", other_entry.as_uri(), "ws://127.0.0.1:9000/devtools/page/other")
    assert (
        unique_content_target(
            (other, expected),
            expected_content_root=content,
            expected_entry=entry,
        )
        == expected
    )
    with pytest.raises(CdpUnavailableError):
        unique_content_target((other,), expected_content_root=content, expected_entry=entry)
    with pytest.raises(CdpProtocolError):
        unique_content_target(
            (expected, expected),
            expected_content_root=content,
            expected_entry=entry,
        )


def test_standard_library_cdp_handles_masking_ping_and_fragmented_response() -> None:
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    port = cast(int, listener.getsockname()[1])
    failures: list[BaseException] = []

    def serve() -> None:
        try:
            connection, _address = listener.accept()
            with connection:
                request = bytearray()
                while b"\r\n\r\n" not in request:
                    request.extend(connection.recv(4096))
                headers = request.decode("ascii")
                key_line = next(
                    line for line in headers.split("\r\n") if line.lower().startswith("sec-websocket-key:")
                )
                key = key_line.split(":", 1)[1].strip()
                accept = base64.b64encode(
                    hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
                ).decode()
                event = json.dumps({"method": "Page.frameStartedLoading", "params": {}}).encode()
                connection.sendall(
                    (
                        "HTTP/1.1 101 Switching Protocols\r\n"
                        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                        f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
                    ).encode("ascii")
                    + _server_frame(event, opcode=1, final=True)
                )
                opcode, payload = _read_client_frame(connection)
                assert opcode == 1
                message = json.loads(payload)
                response = json.dumps({"id": message["id"], "result": {"ok": True}}).encode()
                split = len(response) // 2
                connection.sendall(_server_frame(b"ping", opcode=9, final=True))
                connection.sendall(_server_frame(response[:split], opcode=1, final=False))
                connection.sendall(_server_frame(response[split:], opcode=0, final=True))
                pong_opcode, pong = _read_client_frame(connection)
                assert pong_opcode == 10
                assert pong == b"ping"
        except BaseException as error:  # noqa: BLE001 - 线程失败必须回传主测试。
            failures.append(error)
        finally:
            listener.close()

    thread = threading.Thread(target=serve)
    thread.start()
    with CdpConnection(f"ws://127.0.0.1:{port}/devtools/page/test") as client:
        assert client.call("Runtime.enable") == {"ok": True}
    thread.join(timeout=5)
    assert not thread.is_alive()
    assert not failures


def test_nwjs_public_contract_and_observer_memory_behavior(tmp_path: Path) -> None:
    command = build_nwjs_command(Path("D:/copy/Game.exe"), port=9222, profile=tmp_path / "profile")
    assert command[1:] == (
        "--remote-debugging-address=127.0.0.1",
        "--remote-debugging-port=9222",
        f"--user-data-dir={tmp_path / 'profile'}",
    )
    for name in ("title", "new_game", "dialogue", "menu", "quest_log", "options", "save"):
        assert "supported" in scenario_expression(name)
    with pytest.raises(CdpProtocolError):
        CdpConnection("ws://192.0.2.1:9222/devtools/page/test")
    assert "splice(0, state.events.length)" in OBSERVER_SCRIPT
    assert "200000" not in OBSERVER_SCRIPT
    assert "clearInterval(state.installTimer)" in OBSERVER_SCRIPT
    assert 'window.addEventListener("load"' in OBSERVER_SCRIPT
    assert "Math.min(width, px + allowed)" in OBSERVER_SCRIPT
    assert "px + measuredWidth > contentsWidth" in OBSERVER_SCRIPT
    assert 'measurementStatus = "unverified_control_or_multiline"' in OBSERVER_SCRIPT
    summary = summarize_draws(
        [
            {
                "text": "English",
                "geometry": {
                    "overflowRight": False,
                    "clippingOverflow": True,
                    "measurementStatus": "measured",
                },
            },
            {"text": "中文", "geometry": {"overflowRight": False}},
            {
                "text": "控制\\颜色",
                "geometry": {
                    "overflowRight": None,
                    "measurementStatus": "unverified_control_or_multiline",
                },
            },
        ]
    )
    assert summary["english_candidate_count"] == 1
    assert summary["pixel_overflow_count"] == 1
    assert summary["measurement_unverified_count"] == 1


def test_smoke_keeps_running_after_one_scene_script_exception() -> None:
    class ScenarioConnection:
        def __init__(self) -> None:
            self.calls = 0

        def evaluate(self, _expression: str) -> object:
            self.calls += 1
            if self.calls == 1:
                raise CdpEvaluationError("synthetic scene exception")
            return {"supported": True, "reason": "opened"}

    connection = ScenarioConnection()
    actions = [
        scenario_action(cast(CdpConnection, cast(object, connection)), name) for name in ("new_game", "menu")
    ]
    assert actions == [
        {"supported": False, "reason": "scenario_script_exception"},
        {"supported": True, "reason": "opened"},
    ]
    assert connection.calls == 2

    help_result = _run_script(SCRIPTS / "inspect_nwjs_runtime.py", ["--help"])
    assert "smoke" in help_result.stdout and "observe" in help_result.stdout
