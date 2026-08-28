from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tomllib
from collections.abc import Callable, Iterator, Sequence
from pathlib import Path
from typing import cast

import pytest
from att_skill_tools import core as att_common
from att_skill_tools import core as term_common
from att_toolbox.js import scan_javascript
from att_toolbox.resources import classify_resource_reference, is_resource_file_suffix
from att_toolbox.roundtrip import (
    PlainTextReplacement,
    apply_reviewed_plain_text_lines,
    plain_text_lines,
    replace_reviewed_javascript_literal,
)
from att_toolbox.rpg import iter_string_leaves

ROOT = Path(__file__).resolve().parents[2]
TRANSLATE_SCRIPTS = ROOT / "skills" / "translate-with-att" / "scripts"
PUBLIC_SCRIPT_RELATIVE_PATHS = (
    Path("translate-with-att/scripts/summarize_att_run.py"),
    Path("translate-with-att/scripts/rpg_maker_survey.py"),
    Path("translate-with-att/scripts/translation_preflight.py"),
    Path("translate-with-att/scripts/translation_qa.py"),
    Path("translate-with-att/scripts/inspect_nwjs_runtime.py"),
    Path("translate-with-att/scripts/manage_rpg_maker_fonts.py"),
)
_GENERIC_EVIDENCE_FOR_TEST = (
    "exact_location",
    "active_runtime_consumer",
    "player_visible_non_image_text",
    "builtin_not_owner",
    "rules_cannot_map_reversibly",
    "extract_group_unit_write_back_mapping",
    "unique_owner",
)


def run_script(
    script: Path,
    arguments: Sequence[object],
    *,
    expected: int = 0,
    cwd: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    command = [sys.executable, str(script), *(str(argument) for argument in arguments)]
    result = subprocess.run(
        command,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
        cwd=cwd,
    )
    assert result.returncode == expected, f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    return result


def skill_bytecode_snapshot(root: Path) -> tuple[tuple[str, int, int], ...]:
    entries: list[tuple[str, int, int]] = []
    for path in root.rglob("*"):
        if not path.is_file() or path.suffix not in {".pyc", ".pyo"}:
            continue
        metadata = path.stat()
        entries.append(
            (
                path.relative_to(root).as_posix(),
                metadata.st_size,
                metadata.st_mtime_ns,
            )
        )
    return tuple(sorted(entries))


@pytest.fixture(scope="module")
def clean_public_script_tree(
    tmp_path_factory: pytest.TempPathFactory,
) -> Iterator[tuple[Path, Path]]:
    copied_skills = tmp_path_factory.mktemp("package") / "skills"
    shutil.copytree(
        ROOT / "skills",
        copied_skills,
        ignore=shutil.ignore_patterns("__pycache__", "*.pyc", "*.pyo"),
    )
    outside = tmp_path_factory.mktemp("outside")
    yield copied_skills, outside
    assert skill_bytecode_snapshot(copied_skills) == ()


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")


def write_jsonl(path: Path, values: Sequence[object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(json.dumps(value, ensure_ascii=False) + "\n" for value in values),
        encoding="utf-8",
    )


def assert_four_field_error(result: subprocess.CompletedProcess[str]) -> None:
    for field in ("错误：", "对象：", "原因：", "影响：", "处理办法："):
        assert field in result.stderr


@pytest.fixture
def mv_game(tmp_path: Path) -> Path:
    game = tmp_path / "game"
    data = game / "data"
    plugins = game / "js" / "plugins"
    data.mkdir(parents=True)
    plugins.mkdir(parents=True)
    (game / "js" / "rpg_core.js").write_text("// MV", encoding="utf-8")
    (game / "Game.exe").write_bytes(b"")
    write_json(game / "package.json", {})
    (game / "js" / "plugins.js").write_text(
        'var $plugins = [{"name":"QuestPlugin","status":true,"description":"",'
        '"parameters":{"entries":"[{\\"title\\":\\"Quest Name\\"}]"}}];',
        encoding="utf-8",
    )
    (plugins / "QuestPlugin.js").write_text(
        'const source = "extra.txt";\nwindow.drawText(source, 0, 0);\n', encoding="utf-8"
    )
    (game / "extra.txt").write_text("Visible external text", encoding="utf-8")
    write_json(data / "System.json", {"gameTitle": "Game", "terms": {}, "elements": []})
    write_json(
        data / "Actors.json", [None, {"name": "Hero", "nickname": "", "profile": "", "note": "Actor note"}]
    )
    map_fixture = cast(
        object,
        {
            "displayName": "Town",
            "events": [
                None,
                {
                    "pages": [
                        {
                            "list": [
                                {"code": 101, "parameters": []},
                                {"code": 401, "parameters": ["\\N<Hero>Hello"]},
                                {"code": 356, "parameters": ["Show \\\\TAG[keep] text"]},
                                {"code": 0, "parameters": []},
                            ]
                        }
                    ]
                },
            ],
        },
    )
    write_json(data / "Map001.json", map_fixture)
    write_json(data / "CommonEvents.json", [None])
    write_json(data / "Troops.json", [None])
    write_json(
        data / "QuestEntries.json", [{"title": "Quest Name", "payload": '{"caption":"Quest Caption"}'}]
    )
    return game


def test_resource_reference_classification_keeps_natural_text_and_containers() -> None:
    image = classify_resource_reference(("note",), "Hello.png")
    sentence = classify_resource_reference(("note",), "Please open Hello.png now")
    path_sentence = classify_resource_reference(("note",), "Please open img/pictures/Hello.png")
    spaced_resource = classify_resource_reference(("note",), "img/pictures/Title Picture.png")
    effect = classify_resource_reference(("effectName",), "Explosion")
    effect_file = classify_resource_reference(("file",), "effects/Explosion.efkefc")
    animation = classify_resource_reference(("animationName",), "Slash")
    encrypted = classify_resource_reference(("file",), "img/pictures/Title.rpgmvp")

    assert image is not None and (image.basis, image.resource_kind) == (
        "whole_resource_path",
        "image",
    )
    assert sentence is None
    assert path_sentence is None
    assert spaced_resource is not None and spaced_resource.resource_kind == "image"
    assert effect is not None and effect.resource_kind == "other"
    assert effect_file is not None and effect_file.resource_kind == "other"
    assert animation is not None and animation.resource_kind == "image"
    assert encrypted is not None and encrypted.resource_kind == "encrypted"
    assert classify_resource_reference(("effectName",), "") is None
    assert is_resource_file_suffix(".efkefc")
    assert not any(is_resource_file_suffix(suffix) for suffix in (".txt", ".json", ".js"))


def test_log_summary(tmp_path: Path) -> None:
    log = tmp_path / "run-000001.jsonl"
    records = [
        {
            "timestamp": "2026-08-09T00:00:00Z",
            "sequence": 1,
            "run_id": "run-000001",
            "level": "info",
            "event": "run.started",
            "context": {"locale": "zh-Hans", "engine": "mv", "project": "demo", "command": "translate"},
            "payload": {},
            "message": "started",
        },
        {
            "timestamp": "2026-08-09T00:00:01Z",
            "sequence": 2,
            "run_id": "run-000001",
            "level": "info",
            "event": "phase.started",
            "context": {},
            "payload": {"phase": "planning", "amount": {"kind": "indeterminate"}},
            "message": "planning",
        },
        {
            "timestamp": "2026-08-09T00:00:03Z",
            "sequence": 3,
            "run_id": "run-000001",
            "level": "info",
            "event": "phase.completed",
            "context": {},
            "payload": {"phase": "planning", "amount": {"kind": "determinate", "completed": 1, "total": 1}},
            "message": "planned",
        },
        {
            "timestamp": "2026-08-09T00:00:04Z",
            "sequence": 4,
            "run_id": "run-000001",
            "level": "warn",
            "event": "diagnostic.project_log",
            "context": {},
            "payload": {
                "relation": "observability",
                "object": "项目日志 run-000001.jsonl",
                "reason": "写入 retry.summary 时有 2 条记录未持久化",
                "impact": "业务状态没有修改",
                "help": "检查日志目录后重新运行需要记录的命令",
            },
            "message": "项目日志故障",
        },
        {
            "timestamp": "2026-08-09T00:00:04.5Z",
            "sequence": 5,
            "run_id": "run-000001",
            "level": "warn",
            "event": "translation.finished",
            "context": {},
            "payload": {
                "result": {
                    "kind": "incomplete",
                    "tasks": {
                        "planned": 1,
                        "started": 1,
                        "complete": 0,
                        "partial": 1,
                        "unavailable": 0,
                        "failed": 0,
                        "cancelled": 0,
                        "not_started": 0,
                    },
                    "summary": {
                        "engine": "rpg_maker",
                        "summary": {
                            "accepted_decisions": 0,
                            "written_locations": 0,
                            "remaining_decisions": 1,
                            "remaining_locations": 1,
                            "protocol_diagnostics": 0,
                            "recoverable_request_exhaustions": 0,
                            "request_admission_stopped": False,
                            "retained": 0,
                            "invalidated": 0,
                            "not_applicable": 0,
                            "reused": 0,
                        },
                    },
                }
            },
            "message": "incomplete",
        },
        {
            "timestamp": "2026-08-09T00:00:05Z",
            "sequence": 6,
            "run_id": "run-000001",
            "level": "info",
            "event": "run.finished",
            "context": {},
            "payload": {"result": {"kind": "succeeded"}},
            "message": "finished",
        },
    ]
    log.write_text(
        "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records), encoding="utf-8"
    )
    summary = tmp_path / "summary.json"
    run_script(
        TRANSLATE_SCRIPTS / "summarize_att_run.py",
        ["--log", log, "--output", summary],
    )
    summary_data = json.loads(summary.read_text(encoding="utf-8"))
    assert summary_data["runs"][0]["phases"][0]["duration_seconds"] == 2.0
    assert summary_data["runs"][0]["translation_finished"]["kind"] == "incomplete"
    assert summary_data["runs"][0]["diagnostics"] == [
        {
            "event": "diagnostic.project_log",
            "sequence": 4,
            "relation": "observability",
            "object": "项目日志 run-000001.jsonl",
            "reason": "写入 retry.summary 时有 2 条记录未持久化",
            "impact": "业务状态没有修改",
            "help": "检查日志目录后重新运行需要记录的命令",
        }
    ]


@pytest.mark.parametrize(
    ("engine", "engine_summary", "obsolete_fields"),
    [
        (
            "generic",
            {
                "planned_units": 1,
                "remaining_units": 0,
                "cleared_units": 0,
                "reused_units": 0,
                "accepted_units": 1,
                "written_units": 1,
                "conflicted_units": 0,
                "response_problems": 0,
                "recoverable_request_exhaustions": 0,
                "request_admission_stopped": False,
            },
            (
                "planned_units",
                "remaining_units",
                "recoverable_request_exhaustions",
                "request_admission_stopped",
            ),
        ),
        (
            "rpg_maker",
            {
                "accepted_decisions": 1,
                "written_locations": 1,
                "remaining_decisions": 0,
                "remaining_locations": 0,
                "protocol_diagnostics": 0,
                "recoverable_request_exhaustions": 0,
                "request_admission_stopped": False,
                "retained": 0,
                "invalidated": 0,
                "not_applicable": 0,
                "reused": 0,
            },
            ("request_admission_stopped",),
        ),
    ],
)
def test_log_summary_requires_current_translate_schema_and_task_invariants(
    tmp_path: Path,
    engine: str,
    engine_summary: dict[str, object],
    obsolete_fields: tuple[str, ...],
) -> None:
    valid_tasks = {
        "planned": 1,
        "started": 1,
        "complete": 1,
        "partial": 0,
        "unavailable": 0,
        "failed": 0,
        "cancelled": 0,
        "not_started": 0,
    }

    def run_case(
        name: str,
        tasks: dict[str, int],
        summary: dict[str, object],
        *,
        expected: int = 0,
    ) -> Path:
        log = tmp_path / f"{engine}-{name}.jsonl"
        output = tmp_path / f"{engine}-{name}.json"
        records: list[object] = [
            {
                "timestamp": "2026-08-09T00:00:00Z",
                "sequence": 1,
                "run_id": "run-000001",
                "level": "info",
                "event": "run.started",
                "context": {
                    "locale": "zh-Hans",
                    "engine": engine,
                    "project": "demo",
                    "command": "translate",
                },
                "payload": {},
                "message": "started",
            },
            {
                "timestamp": "2026-08-09T00:00:01Z",
                "sequence": 2,
                "run_id": "run-000001",
                "level": "info",
                "event": "task.started",
                "context": {},
                "payload": {"task": {"ordinal": 1, "total": 1}},
                "message": "task started",
            },
            {
                "timestamp": "2026-08-09T00:00:02Z",
                "sequence": 3,
                "run_id": "run-000001",
                "level": "info",
                "event": "task.finished",
                "context": {},
                "payload": {
                    "task": {"ordinal": 1, "total": 1},
                    "attempts": 1,
                    "outcome": {"kind": "complete"},
                },
                "message": "task finished",
            },
            {
                "timestamp": "2026-08-09T00:00:03Z",
                "sequence": 4,
                "run_id": "run-000001",
                "level": "info",
                "event": "translation.finished",
                "context": {},
                "payload": {
                    "result": {
                        "kind": "complete",
                        "tasks": tasks,
                        "summary": {"engine": engine, "summary": summary},
                    }
                },
                "message": "translation finished",
            },
            {
                "timestamp": "2026-08-09T00:00:04Z",
                "sequence": 5,
                "run_id": "run-000001",
                "level": "info",
                "event": "run.finished",
                "context": {},
                "payload": {"result": {"kind": "succeeded"}},
                "message": "finished",
            },
        ]
        write_jsonl(log, records)
        run_script(
            TRANSLATE_SCRIPTS / "summarize_att_run.py",
            ["--log", log, "--output", output],
            expected=expected,
        )
        if expected != 0:
            assert not output.exists()
        return output

    current_output = run_case("current", valid_tasks, engine_summary)
    current = json.loads(current_output.read_text(encoding="utf-8"))
    assert current["runs"][0]["translation_finished"]["summary"] == {
        "engine": engine,
        "summary": engine_summary,
    }

    obsolete_summary = dict(engine_summary)
    for field in obsolete_fields:
        obsolete_summary.pop(field)
    run_case("obsolete", valid_tasks, obsolete_summary, expected=1)
    run_case("unknown-field", valid_tasks, {**engine_summary, "ignored_field": 0}, expected=1)
    run_case(
        "non-boolean-admission",
        valid_tasks,
        {**engine_summary, "request_admission_stopped": 0},
        expected=1,
    )
    run_case("invalid-started", {**valid_tasks, "started": 2}, engine_summary, expected=1)
    run_case("invalid-planned", {**valid_tasks, "planned": 2}, engine_summary, expected=1)


def test_pytest_imports_do_not_write_skill_bytecode(
    skill_bytecode_at_pytest_start: tuple[tuple[str, int, int], ...],
) -> None:
    assert skill_bytecode_snapshot(ROOT / "skills") == skill_bytecode_at_pytest_start


@pytest.mark.parametrize(
    "relative",
    PUBLIC_SCRIPT_RELATIVE_PATHS,
    ids=lambda path: cast(Path, path).stem,
)
def test_public_script_entry_does_not_write_skill_bytecode(
    relative: Path,
    clean_public_script_tree: tuple[Path, Path],
) -> None:
    copied_skills, outside = clean_public_script_tree
    result = run_script(copied_skills / relative, ["--help"], cwd=outside)
    assert "usage:" in result.stdout


@pytest.mark.parametrize(
    "value",
    [r"\A\\N<(?<speaker>[^>]*)>", '星"\\读', "ordinary text"],
)
def test_toml_string_prefers_literal_strings(value: str) -> None:
    rendered = att_common.toml_string(value)
    assert rendered == f"'{value}'"
    assert tomllib.loads(f"value = {rendered}\n")["value"] == value


@pytest.mark.parametrize("value", ["O'Brien", "line\nbreak", "bad\x07value", "bad\x7fvalue", "'''"])
def test_toml_string_uses_lossless_basic_string_when_literal_is_unsafe(value: str) -> None:
    rendered = att_common.toml_string(value)
    assert rendered.startswith('"') and rendered.endswith('"')
    assert "\x7f" not in rendered
    assert tomllib.loads(f"value = {rendered}\n")["value"] == value


def test_atomic_file_failures_preserve_primary_state(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    target = tmp_path / "result.json"
    target.write_text("old", encoding="utf-8")
    temporary = tmp_path / ".result.json.tmp"

    def fail_replace(_source: Path, _destination: Path) -> None:
        raise PermissionError("publish blocked")

    with monkeypatch.context() as patch:
        patch.setattr(att_common.os, "replace", fail_replace)
        with pytest.raises(att_common.ToolError, match="目标写入或发布失败"):
            att_common.atomic_write_text(target, "new", replace=True)
    assert target.read_text(encoding="utf-8") == "old"
    assert not temporary.exists()

    original_unlink = Path.unlink

    def fail_unlink(path: Path, missing_ok: bool = False) -> None:
        if path == temporary:
            raise PermissionError("cleanup blocked")
        original_unlink(path, missing_ok=missing_ok)

    with monkeypatch.context() as patch:
        patch.setattr(att_common.os, "replace", fail_replace)
        patch.setattr(Path, "unlink", fail_unlink)
        with pytest.raises(att_common.ToolError) as combined:
            att_common.atomic_write_text(target, "new", replace=True)
    assert "目标写入失败" in combined.value.reason
    assert "临时文件清理也失败" in combined.value.reason
    assert target.read_text(encoding="utf-8") == "old"
    assert temporary.read_text(encoding="utf-8") == "new"
    original_unlink(temporary)


def test_atomic_publication_cleanup_reports_applied_state(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    target = tmp_path / "new.json"
    temporary = tmp_path / ".new.json.tmp"
    original_unlink = Path.unlink

    def fail_unlink(path: Path, missing_ok: bool = False) -> None:
        if path == temporary:
            raise PermissionError("cleanup blocked")
        original_unlink(path, missing_ok=missing_ok)

    with monkeypatch.context() as patch:
        patch.setattr(Path, "unlink", fail_unlink)
        with pytest.raises(att_common.ToolError) as captured:
            att_common.atomic_write_text(target, "complete", replace=False)
    assert "已经生效" in captured.value.impact
    assert target.read_text(encoding="utf-8") == "complete"
    assert temporary.read_text(encoding="utf-8") == "complete"
    original_unlink(temporary)


def test_atomic_directory_rollback_and_cleanup_state(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    target = tmp_path / "job"
    target.mkdir()
    (target / "old.txt").write_text("old", encoding="utf-8")
    stage = tmp_path / ".job.tmp"
    previous = tmp_path / ".job.previous"
    original_replace = term_common.os.replace

    def fail_stage_publish(source: Path, destination: Path) -> None:
        if source == stage and destination == target:
            raise PermissionError("publish blocked")
        original_replace(source, destination)

    with monkeypatch.context() as patch:
        patch.setattr(term_common.os, "replace", fail_stage_publish)
        with pytest.raises(PermissionError, match="publish blocked"):
            term_common.atomic_write_directory(target, {"new.txt": "new"}, replace=True)
    assert (target / "old.txt").read_text(encoding="utf-8") == "old"
    assert not stage.exists()
    assert not previous.exists()

    original_rmtree = shutil.rmtree

    def fail_previous_cleanup(path: Path) -> None:
        if path == previous:
            raise PermissionError("cleanup blocked")
        original_rmtree(path)

    with monkeypatch.context() as patch:
        patch.setattr(term_common.shutil, "rmtree", fail_previous_cleanup)
        with pytest.raises(term_common.ToolError) as captured:
            term_common.atomic_write_directory(target, {"new.txt": "new"}, replace=True)
    assert "已经生效" in captured.value.impact
    assert (target / "new.txt").read_text(encoding="utf-8") == "new"
    assert (previous / "old.txt").read_text(encoding="utf-8") == "old"
    original_rmtree(previous)

    invalid_target = tmp_path / "invalid-job"
    with pytest.raises(term_common.ToolError):
        term_common.atomic_write_directory(invalid_target, {"../outside.txt": "bad"}, replace=False)
    assert not invalid_target.exists()
    assert not (tmp_path / ".invalid-job.tmp").exists()
    assert not (tmp_path / "outside.txt").exists()


def test_javascript_lexer_and_unbounded_nested_json() -> None:
    scan = scan_javascript(
        r"""// drawText("comment.txt")
const empty = "";
const escaped = "extra\x2etxt";
const staticTemplate = `static text`;
const dynamicTemplate = `prefix-${drawText("inside.txt")}-suffix`;
const regex = /['\"]data\/x/gi;
const url = /https?:\/\/example[.]com/;
const ratio = total / count;
call() / maybe / flag;
/* "blocked.txt" */
"""
    )
    values = [literal.value for literal in scan.literals]
    assert "comment.txt" not in values
    assert "blocked.txt" not in values
    assert "" in values
    assert "extra.txt" in values
    assert "static text" in values
    assert "data/x" not in values
    assert "blocked.txt" not in values
    assert "total / count" in scan.code
    assert any(warning["kind"] == "ambiguous_slash_treated_as_division" for warning in scan.warnings)
    assert "inside.txt" in values
    assert "drawText" in scan.code
    assert any(warning["kind"] == "dynamic_template_requires_review" for warning in scan.warnings)

    nested: object = {"caption": "Deep text"}
    for _ in range(12):
        nested = json.dumps(nested, ensure_ascii=False)
    leaves = list(
        iter_string_leaves(
            cast(att_common.JsonValue, nested),
            decode_serialized_at=lambda _path, positions: len(positions) < 12,
        )
    )
    assert [(leaf.path, leaf.value, leaf.decoded_layers) for leaf in leaves] == [
        (("caption",), "Deep text", 12)
    ]

    ordinary_text = [
        '"A naturally quoted sentence."',
        '"A quoted first line.\ncontinued without JSON escaping"',
        "{",
        "[0, 0, 0];",
        "[this._actor, 'menu label'",
    ]
    ordinary_leaves = [next(iter_string_leaves(cast(att_common.JsonValue, value))) for value in ordinary_text]
    assert [(leaf.value, leaf.decoded_layers) for leaf in ordinary_leaves] == [
        (value, 0) for value in ordinary_text
    ]


def test_reviewed_roundtrip_helpers_replace_only_exact_approved_locations() -> None:
    javascript = "const a = 'Menu';\nconst b = 'Menu';\n"
    replaced = replace_reviewed_javascript_literal(
        javascript,
        line=2,
        source="Menu",
        translation="菜单's",
        reviewed=True,
    )
    assert replaced.text == "const a = 'Menu';\nconst b = '菜单\\'s';\n"
    with pytest.raises(att_common.ToolError):
        replace_reviewed_javascript_literal(
            javascript,
            line=1,
            source="Menu",
            translation="菜单",
            reviewed=False,
        )
    with pytest.raises(att_common.ToolError):
        replace_reviewed_javascript_literal(
            "draw('Menu', 'Menu');\n",
            line=1,
            source="Menu",
            translation="菜单",
            reviewed=True,
        )

    source = "Title\r\n\r\nBody\nTail"
    assert [(line.line_number, line.text, line.ending) for line in plain_text_lines(source)] == [
        (1, "Title", "\r\n"),
        (2, "", "\r\n"),
        (3, "Body", "\n"),
        (4, "Tail", ""),
    ]
    assert [(line.text, line.ending) for line in plain_text_lines("Before\fAfter\n")] == [
        ("Before\fAfter", "\n")
    ]
    result = apply_reviewed_plain_text_lines(
        source,
        [PlainTextReplacement(line_number=3, source="Body", translation="正文")],
        reviewed=True,
    )
    assert result == "Title\r\n\r\n正文\nTail"
    with pytest.raises(att_common.ToolError):
        apply_reviewed_plain_text_lines(
            source,
            [PlainTextReplacement(line_number=3, source="Changed", translation="正文")],
            reviewed=True,
        )


@pytest.mark.parametrize(
    "value",
    [
        "[-Gallery-] [D]",
        "[-LATECHANGE-]",
        "[13] ... [Ignite]",
        "[[[Before Menu]]]",
        "[[[After Menu]]]\\[Adj]",
        "[[[Quest Eval]]]",
        "[[[Quest Update]]]",
    ],
)
def test_bracketed_player_text_is_not_treated_as_nested_json(value: str) -> None:
    leaves = list(iter_string_leaves(cast(att_common.JsonValue, value)))
    assert [(leaf.path, leaf.value, leaf.decoded_layers) for leaf in leaves] == [((), value, 0)]


def test_serialized_json_array_is_decoded() -> None:
    value = '["Gallery", {"label": "Ignite"}]'
    ordinary = list(iter_string_leaves(cast(att_common.JsonValue, value)))
    assert [(leaf.path, leaf.value, leaf.decoded_layers) for leaf in ordinary] == [((), value, 0)]
    leaves = list(
        iter_string_leaves(
            cast(att_common.JsonValue, value),
            decode_serialized_at=lambda _path, positions: not positions,
        )
    )
    assert [(leaf.path, leaf.value, leaf.decoded_layers) for leaf in leaves] == [
        ((0,), "Gallery", 1),
        ((1, "label"), "Ignite", 1),
    ]


def test_json_looking_player_text_does_not_turn_a_guess_into_source_damage() -> None:
    value = '["Gallery",]'
    leaves = list(iter_string_leaves(cast(att_common.JsonValue, value)))
    assert [(leaf.path, leaf.value, leaf.decoded_layers) for leaf in leaves] == [((), value, 0)]
    with pytest.raises(att_common.ToolError, match="JSON 语法错误"):
        list(
            iter_string_leaves(
                cast(att_common.JsonValue, value),
                decode_serialized_at=lambda _path, positions: not positions,
            )
        )


def test_safe_walk_rejects_internal_external_and_loop_links(tmp_path: Path) -> None:
    created_links: list[Path] = []

    def directory_link(link: Path, target: Path) -> None:
        try:
            link.symlink_to(target, target_is_directory=True)
        except OSError:
            result = subprocess.run(
                ["cmd", "/c", "mklink", "/J", str(link), str(target)],
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode != 0:
                pytest.skip("当前系统不允许测试进程建立 symlink 或 junction")
        created_links.append(link)

    probe = tmp_path / "probe"
    probe.mkdir()
    target = probe / "target"
    target.mkdir()
    (target / "target.txt").write_text("target", encoding="utf-8")
    internal = probe / "internal"
    directory_link(internal, target)
    root_link = tmp_path / "root-link"
    directory_link(root_link, probe)

    external_root = tmp_path / "external-root"
    external_root.mkdir()
    external_target = tmp_path / "outside"
    external_target.mkdir()
    external = external_root / "external"
    directory_link(external, external_target)

    loop_root = tmp_path / "loop-root"
    loop_root.mkdir()
    loop = loop_root / "again"
    directory_link(loop, loop_root)
    try:
        with pytest.raises(att_common.ToolError, match="链接或重解析点"):
            list(att_common.safe_walk_files(probe))
        with pytest.raises(att_common.ToolError, match="扫描根是链接或重解析点"):
            list(att_common.safe_walk_files(root_link))
        with pytest.raises(att_common.ToolError, match="链接或重解析点"):
            list(att_common.safe_walk_files(external_root))
        with pytest.raises(att_common.ToolError, match="链接或重解析点"):
            list(att_common.safe_walk_files(loop_root))
    finally:
        for link in reversed(created_links):
            if link.is_symlink():
                link.unlink()
            elif link.exists():
                link.rmdir()


def test_safe_walk_surfaces_recursive_scan_errors(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    root = tmp_path / "scan-root"
    root.mkdir()

    def denied_walk(*_args: object, **kwargs: object) -> list[tuple[str, list[str], list[str]]]:
        callback = kwargs.get("onerror")
        assert callable(callback)
        cast(Callable[[OSError], object], callback)(PermissionError(13, "denied", str(root / "blocked")))
        return []

    monkeypatch.setattr(att_common.os, "walk", denied_walk)
    with pytest.raises(att_common.ToolError, match="递归扫描失败"):
        list(att_common.safe_walk_files(root))


def test_log_damage_and_diagnostic_schema_are_not_silenced(tmp_path: Path) -> None:
    base = [
        {
            "timestamp": "2026-08-09T00:00:00Z",
            "sequence": 1,
            "run_id": "run-000001",
            "level": "info",
            "event": "run.started",
            "context": {"locale": "zh-Hans", "engine": "mv", "project": "demo", "command": "extract"},
            "payload": {},
            "message": "started",
        },
        {
            "timestamp": "2026-08-09T00:00:01Z",
            "sequence": 2,
            "run_id": "run-000001",
            "level": "info",
            "event": "run.finished",
            "context": {},
            "payload": {"result": {"kind": "succeeded"}},
            "message": "finished",
        },
    ]
    variants: list[list[dict[str, object]]] = []
    invalid_time = json.loads(json.dumps(base))
    invalid_time[0]["timestamp"] = "garbage"
    variants.append(invalid_time)
    mismatched_run = json.loads(json.dumps(base))
    mismatched_run[1]["run_id"] = "run-000002"
    variants.append(mismatched_run)
    invalid_diagnostic = json.loads(json.dumps(base))
    invalid_diagnostic.insert(
        1,
        {
            "timestamp": "2026-08-09T00:00:00.5Z",
            "sequence": 2,
            "run_id": "run-000001",
            "level": "warning",
            "event": "diagnostic.run",
            "context": {},
            "payload": {"object": "x", "reason": "bad"},
            "message": "bad",
        },
    )
    invalid_diagnostic[2]["sequence"] = 3
    variants.append(invalid_diagnostic)
    unknown_run_kind = json.loads(json.dumps(base))
    unknown_run_kind[1]["payload"] = {"result": {"kind": "legacy_success"}}
    variants.append(unknown_run_kind)
    wrong_run_shape = json.loads(json.dumps(base))
    wrong_run_shape[1]["payload"] = {"result": {"kind": "succeeded", "diagnostic": 1}}
    variants.append(wrong_run_shape)
    invalid_phase = json.loads(json.dumps(base))
    invalid_phase.insert(
        1,
        {
            "timestamp": "2026-08-09T00:00:00.5Z",
            "sequence": 2,
            "run_id": "run-000001",
            "level": "info",
            "event": "phase.started",
            "context": {},
            "payload": {"phase": 42, "amount": {"kind": "indeterminate"}},
            "message": "bad phase",
        },
    )
    invalid_phase[2]["sequence"] = 3
    variants.append(invalid_phase)
    invalid_stop = json.loads(json.dumps(base))
    invalid_stop[1:1] = [
        {
            "timestamp": "2026-08-09T00:00:00.2Z",
            "sequence": 2,
            "run_id": "run-000001",
            "level": "info",
            "event": "phase.started",
            "context": {},
            "payload": {"phase": "planning", "amount": {"kind": "indeterminate"}},
            "message": "planning",
        },
        {
            "timestamp": "2026-08-09T00:00:00.5Z",
            "sequence": 3,
            "run_id": "run-000001",
            "level": "error",
            "event": "phase.stopped",
            "context": {},
            "payload": {"phase": "planning", "outcome": {"kind": "unknown"}},
            "message": "bad stop",
        },
    ]
    invalid_stop[3]["sequence"] = 4
    variants.append(invalid_stop)
    unknown_translation = json.loads(json.dumps(base))
    unknown_translation.insert(
        1,
        {
            "timestamp": "2026-08-09T00:00:00.5Z",
            "sequence": 2,
            "run_id": "run-000001",
            "level": "warning",
            "event": "translation.finished",
            "context": {},
            "payload": {"result": {"kind": "partial"}},
            "message": "legacy translation",
        },
    )
    unknown_translation[2]["sequence"] = 3
    variants.append(unknown_translation)
    for unknown_event in (
        "garbage.event",
        "diagnostic.garbage",
        "observability.project_log_degraded",
    ):
        unknown = json.loads(json.dumps(base))
        unknown.insert(
            1,
            {
                "timestamp": "2026-08-09T00:00:00.5Z",
                "sequence": 2,
                "run_id": "run-000001",
                "level": "info",
                "event": unknown_event,
                "context": {},
                "payload": {},
                "message": "unknown",
            },
        )
        unknown[2]["sequence"] = 3
        variants.append(unknown)
    open_phase = json.loads(json.dumps(base))
    open_phase.insert(
        1,
        {
            "timestamp": "2026-08-09T00:00:00.5Z",
            "sequence": 2,
            "run_id": "run-000001",
            "level": "info",
            "event": "phase.started",
            "context": {},
            "payload": {"phase": "planning", "amount": {"kind": "indeterminate"}},
            "message": "planning",
        },
    )
    open_phase[2]["sequence"] = 3
    variants.append(open_phase)
    invalid_level = json.loads(json.dumps(base))
    invalid_level[0]["level"] = "warning"
    variants.append(invalid_level)
    for number, records in enumerate(variants, start=1):
        log = tmp_path / f"damaged-run-{number}.jsonl"
        log.write_text(
            "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records),
            encoding="utf-8",
        )
        output = tmp_path / f"damaged-run-{number}.json"
        failed = run_script(
            TRANSLATE_SCRIPTS / "summarize_att_run.py",
            ["--log", log, "--output", output],
            expected=1,
        )
        assert_four_field_error(failed)
        assert not output.exists()

    duplicate = tmp_path / "duplicate-key.jsonl"
    duplicate.write_text(
        '{"timestamp":"2026-08-09T00:00:00Z","sequence":1,"sequence":1,'
        '"run_id":"run-000001","level":"info","event":"run.finished",'
        '"context":{},"payload":{"result":{"kind":"succeeded"}},"message":"done"}\n',
        encoding="utf-8",
    )
    duplicate_result = run_script(
        TRANSLATE_SCRIPTS / "summarize_att_run.py",
        ["--log", duplicate, "--output", tmp_path / "duplicate-key.json"],
        expected=1,
    )
    assert_four_field_error(duplicate_result)


def test_atomic_directory_rejects_windows_rooted_names(tmp_path: Path) -> None:
    for number, relative in enumerate((r"..\escape.txt", r"C:\escape.txt", r"\rooted.txt"), start=1):
        target = tmp_path / f"unsafe-{number}"
        with pytest.raises(term_common.ToolError):
            term_common.atomic_write_directory(target, {relative: "bad"}, replace=False)
        assert not target.exists()
        assert not target.with_name(f".{target.name}.tmp").exists()


def test_terminal_sanitizer_removes_line_and_direction_controls() -> None:
    cleaned = att_common.sanitize_line("name\n\x1b[31m\u202epath\u2028tail")
    assert cleaned == "name [31m path tail"
    assert all(character not in cleaned for character in ("\n", "\x1b", "\u202e", "\u2028"))
