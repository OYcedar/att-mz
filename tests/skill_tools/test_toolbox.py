from __future__ import annotations

import json
import shutil
import subprocess
import sys
import time
import tomllib
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import cast

import pytest
from att_skill_tools import core as att_common
from att_skill_tools import core as term_common
from att_toolbox.js import scan_javascript
from att_toolbox.rpg import iter_string_leaves
from term_toolbox import grouping as term_grouping

ROOT = Path(__file__).resolve().parents[2]
TRANSLATE_SCRIPTS = ROOT / "skills" / "translate-with-att" / "scripts"
TERM_SCRIPTS = ROOT / "skills" / "extract-game-terminology" / "scripts"
SCRIPT_ENTRIES = [
    *(
        TRANSLATE_SCRIPTS / name
        for name in (
            "inspect_rpg_maker.py",
            "analyze_mv_dialogue.py",
            "analyze_extract_rules.py",
            "analyze_placeholders.py",
            "trace_runtime_text.py",
            "audit_text_ownership.py",
            "summarize_att_run.py",
            "verify_write_back.py",
        )
    ),
    *(
        TERM_SCRIPTS / name
        for name in (
            "prepare_formic_job.py",
            "review_formic_candidates.py",
            "write_terminology.py",
        )
    ),
]


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


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")


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


def test_rpg_inventory_and_rule_tools(mv_game: Path, tmp_path: Path) -> None:
    inventory = tmp_path / "inventory.json"
    run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", mv_game, "--output", inventory],
    )
    inventory_data = json.loads(inventory.read_text(encoding="utf-8"))
    assert inventory_data["engine"] == "mv"
    assert inventory_data["summary"]["active_plugins"] == 1
    assert inventory_data["active_plugins"][0]["script_literals"]["player_text_candidate_count"] == 1

    dialogue = tmp_path / "dialogue.json"
    dialogue_decisions = tmp_path / "dialogue-decisions.json"
    dialogue_rules = tmp_path / "dialogue.toml"
    write_json(dialogue_decisions, {"patterns": [r"\A\\N<(?<speaker>[^>]*)>"]})
    run_script(
        TRANSLATE_SCRIPTS / "analyze_mv_dialogue.py",
        [
            "--game",
            mv_game,
            "--output",
            dialogue,
            "--decisions",
            dialogue_decisions,
            "--rules-output",
            dialogue_rules,
        ],
    )
    assert json.loads(dialogue.read_text(encoding="utf-8"))["recognized_prefix_blocks"] == 1
    assert dialogue_rules.read_text(encoding="utf-8") == (
        "[[rule]]\npattern = '\\A\\\\N<(?<speaker>[^>]*)>'\n"
    )

    candidates = tmp_path / "extract-candidates.json"
    extract_decisions = tmp_path / "extract-decisions.json"
    extract_rules = tmp_path / "extract.toml"
    write_json(
        extract_decisions,
        {
            "rules": [
                {"file": "QuestEntries.json", "path": "[].title"},
                {"plugin": "QuestPlugin", "path": "entries[].title"},
                {"code": 356, "parameter": 0},
            ]
        },
    )
    run_script(
        TRANSLATE_SCRIPTS / "analyze_extract_rules.py",
        [
            "--game",
            mv_game,
            "--output",
            candidates,
            "--decisions",
            extract_decisions,
            "--rules-output",
            extract_rules,
        ],
    )
    candidate_data = json.loads(candidates.read_text(encoding="utf-8"))
    assert any(item["source"].get("plugin") == "QuestPlugin" for item in candidate_data["candidates"])
    assert (
        extract_rules.read_text(encoding="utf-8")
        == """\
[[rule]]
file = 'QuestEntries.json'
path = '[].title'

[[rule]]
plugin = 'QuestPlugin'
path = 'entries[].title'

[[rule]]
code = 356
parameter = 0
"""
    )


def test_inventory_tracks_active_plugin_helper_code(mv_game: Path, tmp_path: Path) -> None:
    (mv_game / "js" / "plugins" / "QuestPlugin.js").write_text(
        'const helper = require("./QuestHelper.js");\n',
        encoding="utf-8",
    )
    (mv_game / "js" / "plugins" / "QuestHelper.js").write_text(
        'const source = "extra.txt"; window.drawText(source, 0, 0);\nrequire("./QuestPlugin.js");\n',
        encoding="utf-8",
    )
    inventory = tmp_path / "helper-inventory.json"
    run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", mv_game, "--output", inventory],
    )
    result = json.loads(inventory.read_text(encoding="utf-8"))
    helper = next(
        item for item in result["external_code_candidates"] if item["path"] == "js/plugins/QuestHelper.js"
    )
    assert helper["player_text_candidate_count"] == 1
    assert helper["active_reference_candidates"] == [
        {
            "plugin": "QuestPlugin",
            "line": 1,
            "exact_static_path_literal": True,
            "loader_call_on_same_line": True,
        }
    ]
    assert any(
        source["source"] == "js/plugins/QuestHelper.js" and source["kind"] == "external_code"
        for source in result["text_sources"]
    )
    trace = tmp_path / "helper-trace.json"
    run_script(
        TRANSLATE_SCRIPTS / "trace_runtime_text.py",
        ["--game", mv_game, "--source", "js/plugins/QuestHelper.js", "--output", trace],
    )
    trace_result = json.loads(trace.read_text(encoding="utf-8"))
    assert trace_result["checks"]["active_runtime_consumer"] == "candidate"
    assert trace_result["checks"]["player_display_call_in_source_code"] == "candidate"
    assert trace_result["generic_enabled"] is False

    indirect_trace = tmp_path / "indirect-extra-trace.json"
    run_script(
        TRANSLATE_SCRIPTS / "trace_runtime_text.py",
        ["--game", mv_game, "--source", "extra.txt", "--output", indirect_trace],
    )
    indirect = json.loads(indirect_trace.read_text(encoding="utf-8"))
    helper_evidence = next(
        item
        for item in indirect["active_consumer_evidence"]
        if item.get("script", "").endswith("QuestHelper.js")
    )
    assert helper_evidence["exact_static_path_references"]
    assert helper_evidence["display_calls"]
    assert any(edge["cycle"] is True for edge in helper_evidence["loader_edges"])
    assert indirect["checks"]["active_runtime_consumer"] == "candidate"


def test_placeholder_trace_and_ownership(mv_game: Path, tmp_path: Path) -> None:
    manual = tmp_path / "manual.toml"
    manual.write_text(
        """\
[[translation]]
id = "Map001.json:event1:page1:dialogue1"
type = "free"
source = ["Use \\\\TAG[keep] and <msg>Hero</msg>"]
translation = []
""",
        encoding="utf-8",
    )
    placeholder_candidates = tmp_path / "placeholder-candidates.json"
    placeholder_decisions = tmp_path / "placeholder-decisions.json"
    placeholder_rules = tmp_path / "placeholders.toml"
    write_json(
        placeholder_decisions, {"rules": [{"pattern": r"\\TAG\[[^]\r\n]*\]", "scopes": ["event_dialogue"]}]}
    )
    run_script(
        TRANSLATE_SCRIPTS / "analyze_placeholders.py",
        [
            "--manual",
            manual,
            "--output",
            placeholder_candidates,
            "--decisions",
            placeholder_decisions,
            "--rules-output",
            placeholder_rules,
        ],
    )
    assert json.loads(placeholder_candidates.read_text(encoding="utf-8"))["custom_candidates"]
    assert (
        placeholder_rules.read_text(encoding="utf-8")
        == r"""[[rule]]
scopes = ['event_dialogue']
pattern = '\\TAG\[[^]\r\n]*\]'
"""
    )

    trace = tmp_path / "trace.json"
    run_script(
        TRANSLATE_SCRIPTS / "trace_runtime_text.py",
        ["--game", mv_game, "--source", "extra.txt", "--output", trace],
    )
    trace_data = json.loads(trace.read_text(encoding="utf-8"))
    assert trace_data["checks"]["active_runtime_consumer"] == "candidate"
    assert trace_data["generic_enabled"] is False

    inventory = tmp_path / "audit-inventory.json"
    decisions = tmp_path / "ownership.json"
    report = tmp_path / "ownership-report.json"
    audit_manual = tmp_path / "audit-manual.toml"
    audit_manual.write_text("translation = []\n", encoding="utf-8")
    write_json(
        inventory,
        {
            "text_sources": [
                {"source": "data/Actors.json:builtin-fields", "kind": "builtin", "builtin": True},
                {"source": "extra.txt", "kind": "external_file", "builtin": False},
            ]
        },
    )
    write_json(
        decisions,
        {
            "sources": [
                {
                    "source": "extra.txt",
                    "owner": "generic",
                    "evidence": {
                        "exact_location": "extra.txt",
                        "active_runtime_consumer": "QuestPlugin is active and references extra.txt",
                        "player_visible_non_image_text": "QuestPlugin passes the loaded value to drawText",
                        "builtin_not_owner": "Builtin matrix does not read extra.txt",
                        "rules_cannot_map_reversibly": "Rules only read data JSON, plugin parameters, and event parameters",
                        "extract_group_unit_write_back_mapping": "one line becomes one Unit and writes to the same line",
                        "unique_owner": "the MV project does not extract extra.txt",
                    },
                }
            ]
        },
    )
    run_script(
        TRANSLATE_SCRIPTS / "audit_text_ownership.py",
        [
            "--inventory",
            inventory,
            "--manual",
            audit_manual,
            "--decisions",
            decisions,
            "--output",
            report,
        ],
    )
    assert json.loads(report.read_text(encoding="utf-8"))["complete"] is True


def test_placeholder_numbered_percent_token_boundaries(tmp_path: Path) -> None:
    manual = tmp_path / "numbered-percent-manual.toml"
    manual.write_text(
        """\
[[translation]]
id = "Map001.json:event1:page1:dialogue1"
type = "free"
source = ["Use %1/%2/%3"]
translation = []

[[translation]]
id = "Map001.json:event1:page1:dialogue2"
type = "free"
source = ["50%", "%", "ordinary text", "%12suffix"]
translation = []
""",
        encoding="utf-8",
    )
    candidates = tmp_path / "numbered-percent-candidates.json"
    run_script(
        TRANSLATE_SCRIPTS / "analyze_placeholders.py",
        ["--manual", manual, "--output", candidates],
    )

    result = json.loads(candidates.read_text(encoding="utf-8"))
    numbered = [item for item in result["custom_candidates"] if item["kind"] == "percent_number"]
    assert numbered == [
        {
            "kind": "percent_number",
            "observed_form": "%1",
            "suggested_pattern": r"%[0-9]+(?![A-Za-z0-9_])",
            "occurrences": 3,
            "locations": ["Map001.json:event1:page1:dialogue1"] * 3,
            "possible_builtin_overlap": False,
            "do_not_select_without_att_check": False,
            "semantics": "unconfirmed",
        }
    ]


def test_log_summary_and_write_back_verification(mv_game: Path, tmp_path: Path) -> None:
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

    baseline = tmp_path / "baseline"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        ["snapshot", "--game", mv_game, "--output", baseline],
    )
    output = tmp_path / "translated-game"
    shutil.copytree(mv_game, output)
    map_path = output / "data" / "Map001.json"
    map_data = json.loads(map_path.read_text(encoding="utf-8"))
    map_data["displayName"] = "城镇"
    write_json(map_path, map_data)
    report = tmp_path / "write-back-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            output,
            "--baseline",
            baseline,
            "--report",
            report,
        ],
    )
    report_data = json.loads(report.read_text(encoding="utf-8"))
    assert report_data["source_unchanged"] is True
    assert report_data["string_values"]["translated_or_changed"] == 1


def test_write_back_accepts_event_text_reflow_but_keeps_event_structure_strict(
    mv_game: Path, tmp_path: Path
) -> None:
    map_path = mv_game / "data" / "Map001.json"
    map_data = json.loads(map_path.read_text(encoding="utf-8"))
    map_data["events"][1]["pages"][0]["list"] = [
        {"code": 101, "indent": 0, "parameters": ["", 0, 0, 2]},
        {"code": 401, "indent": 1, "parameters": ["First"], "lineKind": "first"},
        {"code": 401, "indent": 2, "parameters": ["Second"], "lineKind": "second"},
        {"code": 356, "indent": 0, "parameters": ["Show text"]},
        {"code": 105, "indent": 0, "parameters": [2, False]},
        {"code": 405, "indent": 1, "parameters": ["Scroll"], "lineKind": "scroll"},
        {"code": 0, "indent": 0, "parameters": []},
    ]
    write_json(map_path, map_data)
    baseline = tmp_path / "event-reflow-baseline"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        ["snapshot", "--game", mv_game, "--output", baseline],
    )

    output_game = tmp_path / "event-reflow-output"
    shutil.copytree(mv_game, output_game)
    output_map_path = output_game / "data" / "Map001.json"
    output_map = json.loads(output_map_path.read_text(encoding="utf-8"))
    output_map["events"][1]["pages"][0]["list"] = [
        {"code": 101, "indent": 0, "parameters": ["", 0, 0, 2]},
        {"code": 401, "indent": 1, "parameters": ["译文一"], "lineKind": "first"},
        {"code": 401, "indent": 1, "parameters": ["译文二"], "lineKind": "first"},
        {"code": 401, "indent": 2, "parameters": ["译文三"], "lineKind": "second"},
        {"code": 356, "indent": 0, "parameters": ["Translated command"]},
        {"code": 105, "indent": 0, "parameters": [2, False]},
        {"code": 405, "indent": 1, "parameters": ["滚动一"], "lineKind": "scroll"},
        {"code": 405, "indent": 1, "parameters": ["滚动二"], "lineKind": "scroll"},
        {"code": 0, "indent": 0, "parameters": []},
    ]
    write_json(output_map_path, output_map)
    report = tmp_path / "event-reflow-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            output_game,
            "--baseline",
            baseline,
            "--report",
            report,
        ],
    )
    result = json.loads(report.read_text(encoding="utf-8"))
    assert result["structural_differences"] == 0
    assert result["non_text_value_changes"] == []

    missing_body_game = tmp_path / "missing-event-body-output"
    shutil.copytree(output_game, missing_body_game)
    missing_body_path = missing_body_game / "data" / "Map001.json"
    missing_body = json.loads(missing_body_path.read_text(encoding="utf-8"))
    del missing_body["events"][1]["pages"][0]["list"][1:4]
    write_json(missing_body_path, missing_body)
    missing_body_report = tmp_path / "missing-event-body-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            missing_body_game,
            "--baseline",
            baseline,
            "--report",
            missing_body_report,
        ],
        expected=1,
    )
    assert json.loads(missing_body_report.read_text(encoding="utf-8"))["structural_differences"] > 0

    changed_template_game = tmp_path / "changed-event-template-output"
    shutil.copytree(output_game, changed_template_game)
    changed_template_path = changed_template_game / "data" / "Map001.json"
    changed_template = json.loads(changed_template_path.read_text(encoding="utf-8"))
    changed_template["events"][1]["pages"][0]["list"][2]["lineKind"] = "changed"
    write_json(changed_template_path, changed_template)
    changed_template_report = tmp_path / "changed-event-template-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            changed_template_game,
            "--baseline",
            baseline,
            "--report",
            changed_template_report,
        ],
        expected=1,
    )
    assert json.loads(changed_template_report.read_text(encoding="utf-8"))["structural_differences"] > 0

    changed_command_game = tmp_path / "changed-event-command-output"
    shutil.copytree(output_game, changed_command_game)
    changed_command_path = changed_command_game / "data" / "Map001.json"
    changed_command = json.loads(changed_command_path.read_text(encoding="utf-8"))
    changed_command["events"][1]["pages"][0]["list"][4]["indent"] = 9
    write_json(changed_command_path, changed_command)
    changed_command_report = tmp_path / "changed-event-command-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            changed_command_game,
            "--baseline",
            baseline,
            "--report",
            changed_command_report,
        ],
        expected=1,
    )
    non_text_changes = json.loads(changed_command_report.read_text(encoding="utf-8"))[
        "non_text_value_changes"
    ]
    assert [change["path"] for change in non_text_changes] == [
        "data/Map001.json.events[1].pages[0].list[4].indent"
    ]


@pytest.mark.parametrize(
    ("header_code", "body_code", "header_parameters"),
    [
        (101, 401, ["", 0, 0, 2]),
        (105, 405, [2, False]),
    ],
)
def test_write_back_advances_source_index_for_identical_event_body_templates(
    mv_game: Path,
    tmp_path: Path,
    header_code: int,
    body_code: int,
    header_parameters: list[object],
) -> None:
    map_path = mv_game / "data" / "Map001.json"
    map_data = json.loads(map_path.read_text(encoding="utf-8"))
    map_data["events"][1]["pages"][0]["list"] = [
        {"code": header_code, "indent": 0, "parameters": header_parameters},
        {"code": body_code, "indent": 0, "parameters": ["First"]},
        {"code": body_code, "indent": 0, "parameters": ["Second"]},
        {"code": 0, "indent": 0, "parameters": []},
    ]
    write_json(map_path, map_data)
    baseline = tmp_path / f"identical-template-{header_code}-baseline"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        ["snapshot", "--game", mv_game, "--output", baseline],
    )

    output_game = tmp_path / f"identical-template-{header_code}-output"
    shutil.copytree(mv_game, output_game)
    output_map_path = output_game / "data" / "Map001.json"
    output_map = json.loads(output_map_path.read_text(encoding="utf-8"))
    output_map["events"][1]["pages"][0]["list"][1]["parameters"][0] = "译文"
    write_json(output_map_path, output_map)

    report = tmp_path / f"identical-template-{header_code}-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            output_game,
            "--baseline",
            baseline,
            "--report",
            report,
        ],
    )
    result = json.loads(report.read_text(encoding="utf-8"))
    assert result["string_values"]["translated_or_changed"] == 1
    assert result["structural_differences"] == 0


@pytest.mark.parametrize(
    "relative",
    [Path("Map001lighting.json"), Path("custom") / "Quest.json"],
    ids=["noncanonical-top-level", "nested"],
)
def test_write_back_accepts_identical_unparseable_nonstandard_data_and_rejects_changes(
    mv_game: Path,
    tmp_path: Path,
    relative: Path,
) -> None:
    source = mv_game / "data" / relative
    source.parent.mkdir(parents=True, exist_ok=True)
    source.write_text("0.5,0.6\n", encoding="utf-8")
    baseline = tmp_path / f"invalid-{relative.stem}-baseline"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        ["snapshot", "--game", mv_game, "--output", baseline],
    )

    unchanged_output = tmp_path / f"invalid-{relative.stem}-unchanged"
    shutil.copytree(mv_game, unchanged_output)
    unchanged_report = tmp_path / f"invalid-{relative.stem}-unchanged-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            unchanged_output,
            "--baseline",
            baseline,
            "--report",
            unchanged_report,
        ],
    )
    unchanged_result = json.loads(unchanged_report.read_text(encoding="utf-8"))
    assert unchanged_result["output_json_valid"] is True
    assert unchanged_result["invalid_output_json"] == []

    changed_output = tmp_path / f"invalid-{relative.stem}-changed"
    shutil.copytree(mv_game, changed_output)
    changed_file = changed_output / "data" / relative
    changed_file.write_text("0.7,0.8\n", encoding="utf-8")
    changed_report = tmp_path / f"invalid-{relative.stem}-changed-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            changed_output,
            "--baseline",
            baseline,
            "--report",
            changed_report,
        ],
        expected=1,
    )
    changed_result = json.loads(changed_report.read_text(encoding="utf-8"))
    assert changed_result["output_json_valid"] is False
    assert changed_result["invalid_output_json"][0]["path"] == f"data/{relative.as_posix()}"
    assert "字节不同" in changed_result["invalid_output_json"][0]["reason"]


@pytest.mark.parametrize("file_name", ["Actors.json", "Map001.json"])
def test_write_back_rejects_identical_unparseable_standard_or_canonical_data(
    mv_game: Path,
    tmp_path: Path,
    file_name: str,
) -> None:
    (mv_game / "data" / file_name).write_text("[{\n", encoding="utf-8")
    baseline = tmp_path / f"invalid-strict-{file_name}-baseline"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        ["snapshot", "--game", mv_game, "--output", baseline],
    )
    output_game = tmp_path / f"invalid-strict-{file_name}-output"
    shutil.copytree(mv_game, output_game)
    report = tmp_path / f"invalid-strict-{file_name}-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            output_game,
            "--baseline",
            baseline,
            "--report",
            report,
        ],
        expected=1,
    )
    result = json.loads(report.read_text(encoding="utf-8"))
    assert result["output_json_valid"] is False
    assert result["invalid_output_json"][0]["path"] == f"data/{file_name}"


def test_write_back_baseline_uses_natural_copies_and_detects_source_change(
    mv_game: Path, tmp_path: Path
) -> None:
    baseline = tmp_path / "source-baseline"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        ["snapshot", "--game", mv_game, "--output", baseline],
    )
    manifest_text = (baseline / "baseline.json").read_text(encoding="utf-8")
    assert "sha256" not in manifest_text
    assert "hash" not in manifest_text.casefold()
    assert (baseline / "files" / "data" / "Map001.json").read_bytes() == (
        mv_game / "data" / "Map001.json"
    ).read_bytes()

    output = tmp_path / "source-change-output"
    shutil.copytree(mv_game, output)
    source_map = mv_game / "data" / "Map001.json"
    source_map.write_bytes(source_map.read_bytes() + b"\n")
    report = tmp_path / "source-change-report.json"
    failed = run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            output,
            "--baseline",
            baseline,
            "--report",
            report,
        ],
        expected=1,
    )
    assert_four_field_error(failed)
    report_data = json.loads(report.read_text(encoding="utf-8"))
    assert report_data["source_unchanged"] is False
    assert report_data["changed_source_files"] == ["data/Map001.json"]


def test_terminology_tools_and_four_field_error(tmp_path: Path) -> None:
    manual = tmp_path / "manual.toml"
    manual.write_text(
        """\
[[translation]]
id = "Map001.json:event1:page1:dialogue1"
type = "free"
source = ["星読みの村"]
translation = []

[[translation]]
id = "Map002.json:event1:page1:dialogue1"
type = "free"
source = ["星読みが来た"]
translation = []
""",
        encoding="utf-8",
    )
    job = tmp_path / "formic-job"
    run_script(TERM_SCRIPTS / "prepare_formic_job.py", ["--manual", manual, "--output", job])
    assert (job / "plan.jsonl").read_text(encoding="utf-8").count("\n") == 2
    assert (job / "task.md").is_file()

    formic_out = tmp_path / "formic-out"
    formic_out.mkdir()
    (formic_out / "1.md").write_text("星読み\n村\n", encoding="utf-8")
    (formic_out / "2.md").write_text("- 星読み\n無\n", encoding="utf-8")
    (formic_out / "workers").mkdir()
    (formic_out / "workers" / "ignored.md").write_text("不应读取", encoding="utf-8")
    candidates = tmp_path / "terms-candidates.json"
    run_script(
        TERM_SCRIPTS / "review_formic_candidates.py",
        ["--manual", manual, "--formic-out", formic_out, "--output", candidates],
    )
    candidate_data = json.loads(candidates.read_text(encoding="utf-8"))
    assert [item["term"] for item in candidate_data["candidates"]] == ["星読み"]

    decisions = tmp_path / "terms.json"
    terminology = tmp_path / "terminology.toml"
    write_json(decisions, {"terms": [{"term": "星読み", "translation": "观星者"}]})
    run_script(
        TERM_SCRIPTS / "write_terminology.py",
        ["--input", decisions, "--output", terminology],
    )
    assert "translation = '观星者'" in terminology.read_text(encoding="utf-8")

    duplicate = run_script(
        TERM_SCRIPTS / "write_terminology.py",
        ["--input", decisions, "--output", terminology],
        expected=1,
    )
    assert "对象：" in duplicate.stderr
    assert "原因：" in duplicate.stderr
    assert "影响：" in duplicate.stderr
    assert "处理办法：" in duplicate.stderr


@pytest.mark.parametrize("script", SCRIPT_ENTRIES, ids=lambda path: cast(Path, path).stem)
def test_all_script_help_entries(script: Path, tmp_path: Path) -> None:
    result = run_script(script, ["--help"], cwd=tmp_path)
    assert "usage:" in result.stdout


def test_cli_and_damaged_inputs_fail_without_outputs(mv_game: Path, tmp_path: Path) -> None:
    missing_arguments = run_script(TRANSLATE_SCRIPTS / "inspect_rpg_maker.py", [], expected=2)
    assert_four_field_error(missing_arguments)

    damaged_game = tmp_path / "damaged-game"
    shutil.copytree(mv_game, damaged_game)
    (damaged_game / "js" / "plugins.js").write_text("var $plugins = [{", encoding="utf-8")
    inventory = tmp_path / "damaged-inventory.json"
    damaged_json = run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", damaged_game, "--output", inventory],
        expected=1,
    )
    assert_four_field_error(damaged_json)
    assert not inventory.exists()

    damaged_manual = tmp_path / "damaged.toml"
    damaged_manual.write_text("[[translation]\n", encoding="utf-8")
    job = tmp_path / "damaged-job"
    damaged_toml = run_script(
        TERM_SCRIPTS / "prepare_formic_job.py",
        ["--manual", damaged_manual, "--output", job],
        expected=1,
    )
    assert_four_field_error(damaged_toml)
    assert not job.exists()

    duplicate_rules = tmp_path / "duplicate-rules.json"
    duplicate_rule = {"file": "QuestEntries.json", "path": "[].title"}
    write_json(duplicate_rules, {"rules": [duplicate_rule, duplicate_rule]})
    candidate_output = tmp_path / "duplicate-candidates.json"
    rules_output = tmp_path / "duplicate-rules.toml"
    invalid_review = run_script(
        TRANSLATE_SCRIPTS / "analyze_extract_rules.py",
        [
            "--game",
            mv_game,
            "--output",
            candidate_output,
            "--decisions",
            duplicate_rules,
            "--rules-output",
            rules_output,
        ],
        expected=1,
    )
    assert_four_field_error(invalid_review)
    assert not candidate_output.exists()
    assert not rules_output.exists()


def test_unicode_paths_empty_plugin_keys_and_toml_escaping(mv_game: Path, tmp_path: Path) -> None:
    unicode_game = tmp_path / "夜袭 空格 [MV 游戏]"
    shutil.copytree(mv_game, unicode_game)
    (unicode_game / "js" / "plugins.js").write_text(
        'var $plugins = [{"name":"QuestPlugin","status":true,"description":"",'
        '"parameters":{"":"{\\"\\":\\"星の扉\\"}","source":"extra.txt"}}];',
        encoding="utf-8",
    )
    output_root = tmp_path / "结果 空格 [审核]"
    candidates = output_root / "嵌套候选.json"
    run_script(
        TRANSLATE_SCRIPTS / "analyze_extract_rules.py",
        ["--game", unicode_game, "--output", candidates],
    )
    candidate_data = json.loads(candidates.read_text(encoding="utf-8"))
    plugin_paths = {
        item["path"] for item in candidate_data["candidates"] if item["source"].get("plugin") == "QuestPlugin"
    }
    assert '[""][""]' in plugin_paths

    reviewed = output_root / "术语审核.json"
    terminology = output_root / "术语表.toml"
    term = '星"\\读'
    translation = '观"\\星者'
    write_json(reviewed, {"terms": [{"term": term, "translation": translation}]})
    run_script(TERM_SCRIPTS / "write_terminology.py", ["--input", reviewed, "--output", terminology])
    parsed = tomllib.loads(terminology.read_text(encoding="utf-8"))
    assert parsed["term"][0] == {"term": term, "translation": translation}
    assert f"term = '{term}'" in terminology.read_text(encoding="utf-8")
    assert f"translation = '{translation}'" in terminology.read_text(encoding="utf-8")

    controlled = output_root / "控制字符.json"
    rejected_output = output_root / "不应建立.toml"
    write_json(controlled, {"terms": [{"term": "坏\u0007词", "translation": "坏词"}]})
    control_error = run_script(
        TERM_SCRIPTS / "write_terminology.py",
        ["--input", controlled, "--output", rejected_output],
        expected=1,
    )
    assert_four_field_error(control_error)
    assert not rejected_output.exists()


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


def test_formic_abnormal_candidates_and_missing_units(tmp_path: Path) -> None:
    manual = tmp_path / "manual.toml"
    manual.write_text(
        """\
[[translation]]
id = "Map001.json:event1:page1:dialogue1"
type = "free"
source = ["星読みの村"]
translation = []

[[translation]]
id = "Map002.json:event1:page1:dialogue1"
type = "free"
source = ["星読みが来た"]
translation = []
""",
        encoding="utf-8",
    )
    incomplete_out = tmp_path / "incomplete-out"
    incomplete_out.mkdir()
    (incomplete_out / "1.md").write_text("星読み\n", encoding="utf-8")
    missing_report = tmp_path / "missing-report.json"
    missing = run_script(
        TERM_SCRIPTS / "review_formic_candidates.py",
        ["--manual", manual, "--formic-out", incomplete_out, "--output", missing_report],
        expected=1,
    )
    assert_four_field_error(missing)
    assert not missing_report.exists()

    formic_out = tmp_path / "complete-out"
    formic_out.mkdir()
    (formic_out / "1.md").write_text("星読み\n星読み\n存在しない\n村\n", encoding="utf-8")
    (formic_out / "2.md").write_text("- 星読み\n`星読み`\n無\n", encoding="utf-8")
    report = tmp_path / "candidate-report.json"
    run_script(
        TERM_SCRIPTS / "review_formic_candidates.py",
        ["--manual", manual, "--formic-out", formic_out, "--output", report],
    )
    result = json.loads(report.read_text(encoding="utf-8"))
    assert [item["term"] for item in result["candidates"]] == ["星読み"]
    assert {item["reason"] for item in result["rejected"]} == {
        "duplicate_worker_candidate",
        "not_found_in_corpus",
        "single_occurrence",
    }


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


def test_large_formic_grouping_and_single_pass_occurrence_scan() -> None:
    entries = [
        term_common.ManualEntry(
            readable_id=f"story.jsonl:line{index}:unit1:text",
            translation_type="fixed",
            source=("星読みと魔法剣",),
            translation=(),
        )
        for index in range(1, 60_001)
    ]
    started = time.perf_counter()
    units = term_grouping.build_formic_units(entries)
    occurrences = term_common.scan_term_occurrences(
        ["星読み", "魔法", "魔法剣", "存在しない"],
        entries,
    )
    elapsed = time.perf_counter() - started
    assert sum(len(unit.entries) for unit in units) == 60_000
    assert max(len(term_grouping.render_formic_unit(unit)) for unit in units) <= 24_000
    assert len(units) < 500
    assert occurrences["星読み"].count == 60_000
    assert occurrences["魔法"].count == 60_000
    assert occurrences["魔法剣"].count == 60_000
    assert occurrences["存在しない"].count == 0
    assert elapsed < 10.0


def test_formic_target_does_not_split_or_reject_one_oversized_entry() -> None:
    entry = term_common.ManualEntry(
        readable_id="Map001.json:event1:page1:dialogue1",
        translation_type="free",
        source=("星" * 25_000,),
        translation=(),
    )
    units = term_grouping.build_formic_units([entry])
    assert len(units) == 1
    assert units[0].entries == (entry,)
    assert len(term_grouping.render_formic_unit(units[0])) > 24_000


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
    leaves = list(iter_string_leaves(cast(att_common.JsonValue, nested)))
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
    leaves = list(iter_string_leaves(cast(att_common.JsonValue, value)))
    assert [(leaf.path, leaf.value, leaf.decoded_layers) for leaf in leaves] == [
        ((0,), "Gallery", 1),
        ((1, "label"), "Ignite", 1),
    ]


def test_damaged_serialized_json_array_fails_strictly() -> None:
    with pytest.raises(att_common.ToolError, match="JSON 语法错误"):
        list(iter_string_leaves(cast(att_common.JsonValue, '["Gallery",]')))


def test_trace_keeps_dynamic_template_expression_evidence(mv_game: Path, tmp_path: Path) -> None:
    (mv_game / "js" / "plugins" / "QuestPlugin.js").write_text(
        'const shown = `${condition ? drawText("extra.txt") : ""}`;\n',
        encoding="utf-8",
    )
    report = tmp_path / "dynamic-trace.json"
    run_script(
        TRANSLATE_SCRIPTS / "trace_runtime_text.py",
        ["--game", mv_game, "--source", "extra.txt", "--output", report],
    )
    result = json.loads(report.read_text(encoding="utf-8"))
    assert result["checks"]["active_runtime_consumer"] == "candidate"
    evidence = result["active_consumer_evidence"][0]
    assert evidence["exact_static_path_references"][0]["line"] == 1
    assert evidence["lexer_warnings"][0]["kind"] == "dynamic_template_requires_review"


def test_rpg_discovery_and_schema_damage_are_explicit(mv_game: Path, tmp_path: Path) -> None:
    dual = tmp_path / "dual-root"
    shutil.copytree(mv_game, dual)
    shutil.copytree(mv_game, dual / "www")
    ambiguous = run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", dual, "--output", tmp_path / "ambiguous.json"],
        expected=1,
    )
    assert "多个可能的内容根" in ambiguous.stderr

    missing_core = tmp_path / "missing-core"
    shutil.copytree(mv_game, missing_core)
    (missing_core / "js" / "rpg_core.js").unlink()
    (missing_core / "js" / "plugins.js").write_text(
        'var $plugins = [{"name":"QuestPlugin","status":true,'
        '"description":"PluginManager.registerCommand","parameters":{}}];',
        encoding="utf-8",
    )
    unknown_engine = run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", missing_core, "--output", tmp_path / "unknown.json"],
        expected=1,
    )
    assert "无法确认 MV 或 MZ" in unknown_engine.stderr

    damaged = tmp_path / "damaged-entry"
    shutil.copytree(mv_game, damaged)
    actors = json.loads((damaged / "data" / "Actors.json").read_text(encoding="utf-8"))
    actors.append(None)
    write_json(damaged / "data" / "Actors.json", actors)
    damaged_result = run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", damaged, "--output", tmp_path / "damaged-entry.json"],
        expected=1,
    )
    assert "只有 index 0 可以是 null" in damaged_result.stderr

    empty_dialogue = tmp_path / "empty-dialogue"
    shutil.copytree(mv_game, empty_dialogue)
    map_path = empty_dialogue / "data" / "Map001.json"
    map_data = json.loads(map_path.read_text(encoding="utf-8"))
    map_data["events"][1]["pages"][0]["list"][1]["parameters"] = [""]
    write_json(map_path, map_data)
    empty_output = tmp_path / "empty-dialogue.json"
    run_script(
        TRANSLATE_SCRIPTS / "analyze_mv_dialogue.py",
        ["--game", empty_dialogue, "--output", empty_output],
    )
    empty_result = json.loads(empty_output.read_text(encoding="utf-8"))
    empty_facts = {item["shape"]: item["occurrences"] for item in empty_result["unrecognized_prefixes"]}
    assert empty_facts["empty_first_line"] == 1


def test_map_custom_fields_and_mv_dialogue_counterexamples(mv_game: Path, tmp_path: Path) -> None:
    map_path = mv_game / "data" / "Map001.json"
    map_data = json.loads(map_path.read_text(encoding="utf-8"))
    event = map_data["events"][1]
    event["customCaption"] = "Visible custom field"
    event["pages"][0]["list"] = [
        {"code": 101, "parameters": []},
        {"code": 401, "parameters": ["【Alice】Hello"]},
        {"code": 101, "parameters": []},
        {"code": 401, "parameters": ["Alice：Hello"]},
        {"code": 101, "parameters": []},
        {"code": 401, "parameters": [r"\N[145]"]},
        {"code": 101, "parameters": []},
        {"code": 401, "parameters": [r"\N<>Body"]},
        {"code": 0, "parameters": []},
    ]
    write_json(map_path, map_data)

    rule_candidates = tmp_path / "map-rules.json"
    run_script(
        TRANSLATE_SCRIPTS / "analyze_extract_rules.py",
        ["--game", mv_game, "--output", rule_candidates],
    )
    rules = json.loads(rule_candidates.read_text(encoding="utf-8"))["candidates"]
    assert any("customCaption" in item["path"] for item in rules)

    dialogue = tmp_path / "dialogue-counterexamples.json"
    run_script(
        TRANSLATE_SCRIPTS / "analyze_mv_dialogue.py",
        ["--game", mv_game, "--output", dialogue],
    )
    result = json.loads(dialogue.read_text(encoding="utf-8"))
    shapes = {item["shape"] for item in result["unrecognized_prefixes"]}
    assert {"corner_brackets:【{text}】", "colon_label:{text}："} <= shapes
    conflicts = {fact["kind"] for candidate in result["candidates"] for fact in candidate["conflict_facts"]}
    assert {
        "ordinary_control_code_shape_possible",
        "blank_speaker",
        "marker_and_body_on_same_line",
    } <= conflicts


def test_nested_data_is_inventoried_but_not_flattened_into_rules(mv_game: Path, tmp_path: Path) -> None:
    nested = mv_game / "data" / "custom"
    nested.mkdir()
    write_json(nested / "Quest.json", {"caption": "Nested Quest", "price": 10})
    (nested / "notes.md").write_text("Player-facing notes", encoding="utf-8")

    inventory = tmp_path / "nested-inventory.json"
    run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", mv_game, "--output", inventory],
    )
    inventory_data = json.loads(inventory.read_text(encoding="utf-8"))
    nested_fact = next(
        fact for fact in inventory_data["data_candidates"] if fact["path"] == "data/custom/Quest.json"
    )
    assert nested_fact["candidate_string_count"] == 1
    assert nested_fact["rules_supported"] is False
    assert any(
        source["source"] == "data/custom/Quest.json" and source["rules_supported"] is False
        for source in inventory_data["text_sources"]
    )
    assert any(source["source"] == "data/custom/notes.md" for source in inventory_data["text_sources"])

    rules = tmp_path / "nested-rules.json"
    run_script(
        TRANSLATE_SCRIPTS / "analyze_extract_rules.py",
        ["--game", mv_game, "--output", rules],
    )
    rule_data = json.loads(rules.read_text(encoding="utf-8"))
    unsupported = next(
        fact for fact in rule_data["unsupported_sources"] if fact["source"] == "data/custom/Quest.json"
    )
    assert unsupported["candidate_paths"] == [{"path": "caption", "occurrences": 1}]
    assert not any(candidate["source"].get("file") == "Quest.json" for candidate in rule_data["candidates"])

    baseline = tmp_path / "nested-baseline"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        ["snapshot", "--game", mv_game, "--output", baseline],
    )
    output_game = tmp_path / "nested-output"
    shutil.copytree(mv_game, output_game)
    write_json(output_game / "data" / "custom" / "Quest.json", {"caption": "Nested Quest", "price": 99})
    report = tmp_path / "nested-report.json"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            output_game,
            "--baseline",
            baseline,
            "--report",
            report,
        ],
        expected=1,
    )
    report_data = json.loads(report.read_text(encoding="utf-8"))
    assert {change["path"] for change in report_data["non_text_value_changes"]} == {
        "data/custom/Quest.json.price"
    }


def test_noncanonical_invalid_data_json_remains_unresolved(mv_game: Path, tmp_path: Path) -> None:
    source = mv_game / "data" / "Map001lighting.json"
    source.write_text("0.5,0.6\n", encoding="utf-8")

    inventory = tmp_path / "invalid-custom-inventory.json"
    run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", mv_game, "--output", inventory],
    )
    inventory_data = json.loads(inventory.read_text(encoding="utf-8"))
    inventory_fact = next(
        fact for fact in inventory_data["data_candidates"] if fact["source"] == "data/Map001lighting.json"
    )
    assert inventory_fact["kind"] == "unparsed_data_json"
    assert inventory_fact["candidate_string_count"] is None
    assert inventory_fact["rules_supported"] is False
    assert "外层内容无法按 JSON 解析" in inventory_fact["reason"]
    assert any(fact["source"] == "data/Map001lighting.json" for fact in inventory_data["text_sources"])

    rules = tmp_path / "invalid-custom-rules.json"
    run_script(
        TRANSLATE_SCRIPTS / "analyze_extract_rules.py",
        ["--game", mv_game, "--output", rules],
    )
    rule_data = json.loads(rules.read_text(encoding="utf-8"))
    unsupported = next(
        fact for fact in rule_data["unsupported_sources"] if fact["source"] == "data/Map001lighting.json"
    )
    assert unsupported["kind"] == "unparsed_data_json"
    assert unsupported["candidate_string_count"] is None
    assert unsupported["candidate_paths"] == []
    assert unsupported["rules_supported"] is False

    manual = tmp_path / "invalid-custom-manual.toml"
    decisions = tmp_path / "invalid-custom-decisions.json"
    ownership = tmp_path / "invalid-custom-ownership.json"
    manual.write_text("translation = []\n", encoding="utf-8")
    write_json(decisions, {"sources": []})
    run_script(
        TRANSLATE_SCRIPTS / "audit_text_ownership.py",
        [
            "--inventory",
            inventory,
            "--manual",
            manual,
            "--decisions",
            decisions,
            "--output",
            ownership,
        ],
        expected=1,
    )
    ownership_data = json.loads(ownership.read_text(encoding="utf-8"))
    assert ownership_data["complete"] is False
    assert "data/Map001lighting.json" in ownership_data["unresolved_sources"]


@pytest.mark.parametrize("file_name", ["Actors.json", "Map001.json"])
@pytest.mark.parametrize("script_name", ["inspect_rpg_maker.py", "analyze_extract_rules.py"])
def test_standard_and_canonical_invalid_data_json_still_fail_strictly(
    mv_game: Path,
    tmp_path: Path,
    script_name: str,
    file_name: str,
) -> None:
    (mv_game / "data" / file_name).write_text("[{\n", encoding="utf-8")
    output = tmp_path / f"{Path(script_name).stem}-invalid-{file_name}"
    failed = run_script(
        TRANSLATE_SCRIPTS / script_name,
        ["--game", mv_game, "--output", output],
        expected=1,
    )
    assert "JSON 语法错误" in failed.stderr
    assert not output.exists()


@pytest.mark.parametrize("script_name", ["inspect_rpg_maker.py", "analyze_extract_rules.py"])
def test_damaged_nested_json_in_custom_data_still_fails_strictly(
    mv_game: Path,
    tmp_path: Path,
    script_name: str,
) -> None:
    write_json(mv_game / "data" / "Custom.json", {"payload": '["broken",]'})
    output = tmp_path / f"{Path(script_name).stem}-invalid-nested.json"
    failed = run_script(
        TRANSLATE_SCRIPTS / script_name,
        ["--game", mv_game, "--output", output],
        expected=1,
    )
    assert "JSON 语法错误" in failed.stderr
    assert not output.exists()


@pytest.mark.parametrize(
    "plugins_text, expected_reason",
    [
        (
            (
                'var $plugins = [{"name":"QuestPlugin","name":"Other","status":true,'
                '"description":"","parameters":{}}];'
            ),
            "重复 key",
        ),
        (
            (
                'var $plugins = [{"name":"QuestPlugin","status":true,"description":"",'
                '"parameters":{"payload":"{\\"caption\\":\\"A\\",\\"caption\\":\\"B\\"}"}}];'
            ),
            "重复 key",
        ),
        (
            (
                'var $plugins = [{"name":"QuestPlugin","status":true,"description":"",'
                '"parameters":{"payload":"{\\"caption\\":"}}];'
            ),
            "JSON 语法错误",
        ),
        (
            (
                'var $plugins = [{"name":"QuestPlugin","status":true,"description":"",'
                '"parameters":{"payload":"[NaN]"}}];'
            ),
            "非有限数字",
        ),
    ],
)
def test_plugin_and_nested_json_use_strict_decoder(
    mv_game: Path,
    tmp_path: Path,
    plugins_text: str,
    expected_reason: str,
) -> None:
    (mv_game / "js" / "plugins.js").write_text(plugins_text, encoding="utf-8")
    failed = run_script(
        TRANSLATE_SCRIPTS / "analyze_extract_rules.py",
        ["--game", mv_game, "--output", tmp_path / "strict-json.json"],
        expected=1,
    )
    assert expected_reason in failed.stderr
    assert not (tmp_path / "strict-json.json").exists()


def test_placeholder_overlap_and_multi_output_preflight(tmp_path: Path) -> None:
    manual = tmp_path / "placeholder-manual.toml"
    manual.write_text(
        """[[translation]]
id = "Map001.json:event1:page1:dialogue1"
type = "free"
source = ["Value \\\\n[123]"]
translation = []

[[translation]]
id = "Map001.json:event1:page1:dialogue2"
type = "free"
source = ['\\TAG[value', ']', '\\NAME<value', '>', '{{value', '}}', '${value', '}', '<tag value', '>']
translation = []
""",
        encoding="utf-8",
    )
    candidates = tmp_path / "placeholder-overlap.json"
    decisions = tmp_path / "placeholder-overlap-decisions.json"
    rules = tmp_path / "placeholder-overlap.toml"
    write_json(decisions, {"rules": [{"pattern": r"\\n\[[^]\r\n]*\]"}]})
    run_script(
        TRANSLATE_SCRIPTS / "analyze_placeholders.py",
        [
            "--manual",
            manual,
            "--output",
            candidates,
            "--decisions",
            decisions,
            "--rules-output",
            rules,
        ],
    )
    result = json.loads(candidates.read_text(encoding="utf-8"))
    assert any(item["possible_builtin_overlap"] is True for item in result["custom_candidates"])
    assert any(item["reason"] == "possible_builtin_overlap" for item in result["overlap_risks"])
    assert {item["kind"] for item in result["cross_line_risks"]} == {
        "backslash_bracket",
        "backslash_angle",
        "mustache",
        "template",
        "angle_tag",
    }
    assert "[[rule]]" in rules.read_text(encoding="utf-8")

    blocked_candidates = tmp_path / "blocked-candidates.json"
    blocked_rules = tmp_path / "blocked-rules.toml"
    (tmp_path / ".blocked-rules.toml.tmp").write_text("occupied", encoding="utf-8")
    blocked = run_script(
        TRANSLATE_SCRIPTS / "analyze_placeholders.py",
        [
            "--manual",
            manual,
            "--output",
            blocked_candidates,
            "--decisions",
            decisions,
            "--rules-output",
            blocked_rules,
        ],
        expected=1,
    )
    assert_four_field_error(blocked)
    assert not blocked_candidates.exists()
    assert not blocked_rules.exists()


def test_terminology_exact_whitespace_and_control_contract(tmp_path: Path) -> None:
    valid = tmp_path / "valid-terms.json"
    output = tmp_path / "valid-terms.toml"
    write_json(
        valid,
        {
            "terms": [
                {
                    "term": '星"\\读',
                    "translation": '观"\\星',
                    "triggers": ["星\n読み"],
                }
            ]
        },
    )
    run_script(TERM_SCRIPTS / "write_terminology.py", ["--input", valid, "--output", output])
    parsed = tomllib.loads(output.read_text(encoding="utf-8"))["term"][0]
    assert parsed["triggers"] == ["星\n読み"]

    invalid_items = [
        {"term": "   ", "translation": "译"},
        {"term": " 星", "translation": "译"},
        {"term": "星", "translation": "译 "},
        {"term": "星\u202e", "translation": "译"},
        {"term": "星", "translation": "译", "triggers": ["星\r读"]},
        {"term": "星", "translation": "译", "triggers": ["星\x00读"]},
        {"term": "星", "translation": "译", "triggers": ["星\u0085读"]},
        {"term": "星", "translation": "译", "triggers": ["星\u2028读"]},
    ]
    for number, item in enumerate(invalid_items, start=1):
        reviewed = tmp_path / f"invalid-term-{number}.json"
        rejected = tmp_path / f"invalid-term-{number}.toml"
        write_json(reviewed, {"terms": [item]})
        result = run_script(
            TERM_SCRIPTS / "write_terminology.py",
            ["--input", reviewed, "--output", rejected],
            expected=1,
        )
        assert_four_field_error(result)
        assert not rejected.exists()


def test_output_boundaries_and_symlink_reads(mv_game: Path, tmp_path: Path) -> None:
    system = mv_game / "data" / "System.json"
    original_system = system.read_bytes()
    in_game = run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", mv_game, "--output", system, "--replace"],
        expected=1,
    )
    assert_four_field_error(in_game)
    assert system.read_bytes() == original_system

    manual_root = tmp_path / "manual-root"
    manual_root.mkdir()
    manual = manual_root / "manual.toml"
    manual.write_text("translation = []\n", encoding="utf-8")
    same_file = run_script(
        TRANSLATE_SCRIPTS / "analyze_placeholders.py",
        ["--manual", manual, "--output", manual, "--replace"],
        expected=1,
    )
    assert_four_field_error(same_file)
    assert manual.read_text(encoding="utf-8") == "translation = []\n"
    ancestor = run_script(
        TERM_SCRIPTS / "prepare_formic_job.py",
        ["--manual", manual, "--output", manual_root, "--replace"],
        expected=1,
    )
    assert_four_field_error(ancestor)

    linked_game = tmp_path / "linked-game"
    try:
        linked_game.symlink_to(mv_game, target_is_directory=True)
    except OSError:
        return
    linked_input = run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", linked_game, "--output", tmp_path / "linked-game-inventory.json"],
        expected=1,
    )
    assert "链接或重解析点" in linked_input.stderr
    linked_output = run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", mv_game, "--output", linked_game / "unsafe.json", "--replace"],
        expected=1,
    )
    assert_four_field_error(linked_output)
    assert not (mv_game / "unsafe.json").exists()

    external = tmp_path / "outside.txt"
    external.write_text("outside", encoding="utf-8")
    linked = mv_game / "linked.txt"
    linked.symlink_to(external)
    inventory = tmp_path / "linked-inventory.json"
    linked_result = run_script(
        TRANSLATE_SCRIPTS / "inspect_rpg_maker.py",
        ["--game", mv_game, "--output", inventory],
        expected=1,
    )
    assert "链接或重解析点" in linked_result.stderr
    assert not inventory.exists()


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


def test_write_back_detects_all_non_text_scalar_changes(mv_game: Path, tmp_path: Path) -> None:
    write_json(
        mv_game / "data" / "Meta.json",
        {"price": 10, "enabled": True, "optional": None, "values": [1, False, None]},
    )
    baseline = tmp_path / "non-text-baseline"
    run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        ["snapshot", "--game", mv_game, "--output", baseline],
    )
    output_game = tmp_path / "non-text-output"
    shutil.copytree(mv_game, output_game)
    meta = json.loads((output_game / "data" / "Meta.json").read_text(encoding="utf-8"))
    meta.update({"price": 999, "enabled": False, "optional": 1, "values": [2, True, 0]})
    write_json(output_game / "data" / "Meta.json", meta)
    report = tmp_path / "non-text-report.json"
    failed = run_script(
        TRANSLATE_SCRIPTS / "verify_write_back.py",
        [
            "verify",
            "--game",
            mv_game,
            "--output-root",
            output_game,
            "--baseline",
            baseline,
            "--report",
            report,
        ],
        expected=1,
    )
    assert_four_field_error(failed)
    changes = json.loads(report.read_text(encoding="utf-8"))["non_text_value_changes"]
    assert {item["path"] for item in changes} == {
        "data/Meta.json.price",
        "data/Meta.json.enabled",
        "data/Meta.json.optional",
        "data/Meta.json.values[0]",
        "data/Meta.json.values[1]",
        "data/Meta.json.values[2]",
    }


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


def test_ownership_uses_final_manual_and_rejects_blank_generic_evidence(tmp_path: Path) -> None:
    inventory = tmp_path / "inventory.json"
    manual = tmp_path / "manual.toml"
    decisions = tmp_path / "decisions.json"
    report = tmp_path / "ownership.json"
    write_json(
        inventory,
        {
            "text_sources": [
                {"source": "data/Actors.json:builtin-fields", "kind": "builtin", "builtin": True},
                {"source": "data/QuestData.json", "kind": "custom_data", "builtin": False},
            ]
        },
    )
    manual.write_text(
        """[[translation]]
id = "Actors.json:1:name"
type = "fixed"
source = ["Hero"]
translation = []

[[translation]]
id = "QuestData.json:quests:4:title"
type = "fixed"
source = ["Quest"]
translation = []
""",
        encoding="utf-8",
    )
    write_json(
        decisions,
        {
            "sources": [
                {
                    "source": "data/QuestData.json",
                    "owner": "rules",
                    "evidence": "最终 Extract Rules 命中该公开位置",
                }
            ]
        },
    )
    run_script(
        TRANSLATE_SCRIPTS / "audit_text_ownership.py",
        ["--inventory", inventory, "--manual", manual, "--decisions", decisions, "--output", report],
    )
    result = json.loads(report.read_text(encoding="utf-8"))
    assert result["complete"] is True
    assert {row["source"]: row["extracted_entry_count"] for row in result["sources"]} == {
        "data/Actors.json:builtin-fields": 1,
        "data/QuestData.json": 1,
    }

    generic_inventory = tmp_path / "generic-conflict-inventory.json"
    generic_manual = tmp_path / "generic-conflict-manual.toml"
    generic_decisions = tmp_path / "generic-conflict-decisions.json"
    generic_report = tmp_path / "generic-conflict-report.json"
    write_json(
        generic_inventory,
        {"text_sources": [{"source": "story.jsonl", "kind": "external_file", "builtin": False}]},
    )
    generic_manual.write_text(
        """[[translation]]
id = "story.jsonl:line1:unit1:text"
type = "fixed"
source = ["Story"]
translation = []
""",
        encoding="utf-8",
    )
    write_json(
        generic_decisions,
        {
            "sources": [
                {
                    "source": "story.jsonl",
                    "owner": "generic",
                    "manual_prefixes": ["story.jsonl:"],
                    "evidence": {
                        "exact_location": "story.jsonl",
                        "active_runtime_consumer": "active loader",
                        "player_visible_non_image_text": "visible text call",
                        "builtin_not_owner": "not a Builtin source",
                        "rules_cannot_map_reversibly": "not a Rules data source",
                        "extract_group_unit_write_back_mapping": "mapping reviewed",
                        "unique_owner": "Generic only",
                    },
                }
            ]
        },
    )
    generic_failed = run_script(
        TRANSLATE_SCRIPTS / "audit_text_ownership.py",
        [
            "--inventory",
            generic_inventory,
            "--manual",
            generic_manual,
            "--decisions",
            generic_decisions,
            "--output",
            generic_report,
        ],
        expected=1,
    )
    assert_four_field_error(generic_failed)
    generic_result = json.loads(generic_report.read_text(encoding="utf-8"))
    assert generic_result["complete"] is False
    assert generic_result["ownership_conflicts"][0]["owner"] == "generic"

    unsupported_inventory = tmp_path / "unsupported-rules-inventory.json"
    unsupported_manual = tmp_path / "unsupported-rules-manual.toml"
    unsupported_decisions = tmp_path / "unsupported-rules-decisions.json"
    unsupported_report = tmp_path / "unsupported-rules-report.json"
    write_json(
        unsupported_inventory,
        {
            "text_sources": [
                {
                    "source": "data/custom/Quest.json",
                    "kind": "nested_data_json",
                    "builtin": False,
                    "rules_supported": False,
                }
            ]
        },
    )
    unsupported_manual.write_text("translation = []\n", encoding="utf-8")
    write_json(
        unsupported_decisions,
        {
            "sources": [
                {
                    "source": "data/custom/Quest.json",
                    "owner": "rules",
                    "evidence": "nested candidate reviewed",
                    "zero_text_reason": "expected no current text",
                }
            ]
        },
    )
    run_script(
        TRANSLATE_SCRIPTS / "audit_text_ownership.py",
        [
            "--inventory",
            unsupported_inventory,
            "--manual",
            unsupported_manual,
            "--decisions",
            unsupported_decisions,
            "--output",
            unsupported_report,
        ],
        expected=1,
    )
    unsupported_result = json.loads(unsupported_report.read_text(encoding="utf-8"))
    assert unsupported_result["complete"] is False
    assert unsupported_result["ownership_conflicts"][0]["owner"] == "rules"

    blank_inventory = tmp_path / "blank-inventory.json"
    blank_manual = tmp_path / "blank-manual.toml"
    blank_decisions = tmp_path / "blank-decisions.json"
    write_json(
        blank_inventory,
        {"text_sources": [{"source": "story.jsonl", "kind": "external_file", "builtin": False}]},
    )
    blank_manual.write_text("translation = []\n", encoding="utf-8")
    write_json(
        blank_decisions,
        {
            "sources": [
                {
                    "source": "story.jsonl",
                    "owner": "generic",
                    "evidence": {
                        field: " "
                        for field in (
                            "exact_location",
                            "active_runtime_consumer",
                            "player_visible_non_image_text",
                            "builtin_not_owner",
                            "rules_cannot_map_reversibly",
                            "extract_group_unit_write_back_mapping",
                            "unique_owner",
                        )
                    },
                }
            ]
        },
    )
    blank = run_script(
        TRANSLATE_SCRIPTS / "audit_text_ownership.py",
        [
            "--inventory",
            blank_inventory,
            "--manual",
            blank_manual,
            "--decisions",
            blank_decisions,
            "--output",
            tmp_path / "blank-report.json",
        ],
        expected=1,
    )
    assert_four_field_error(blank)


def test_ownership_indexes_many_custom_data_sources(tmp_path: Path) -> None:
    source_count = 500
    entry_count = 2_000
    inventory = tmp_path / "large-inventory.json"
    manual = tmp_path / "large-manual.toml"
    decisions = tmp_path / "large-decisions.json"
    report = tmp_path / "large-ownership.json"
    sources = [f"data/Custom{number:04d}.json" for number in range(source_count)]
    write_json(
        inventory,
        {
            "text_sources": [
                {"source": source, "kind": "custom_data", "builtin": False}
                for source in sources
            ]
        },
    )
    write_json(
        decisions,
        {
            "sources": [
                {
                    "source": source,
                    "owner": "rules" if number == 0 else "excluded",
                    "evidence": "最终 Rules 命中" if number == 0 else "已确认没有玩家可见文字",
                }
                for number, source in enumerate(sources)
            ]
        },
    )
    manual.write_text(
        "\n".join(
            f'''[[translation]]
id = "Custom0000.json:rows:{number}:text"
type = "fixed"
source = ["Text {number}"]
translation = []
'''
            for number in range(entry_count)
        ),
        encoding="utf-8",
    )

    started = time.perf_counter()
    run_script(
        TRANSLATE_SCRIPTS / "audit_text_ownership.py",
        ["--inventory", inventory, "--manual", manual, "--decisions", decisions, "--output", report],
    )
    elapsed = time.perf_counter() - started

    result = json.loads(report.read_text(encoding="utf-8"))
    assert result["complete"] is True
    assert result["counts"] == {
        "builtin": 0,
        "rules": 1,
        "generic": 0,
        "excluded": source_count - 1,
        "unresolved": 0,
    }
    assert result["sources"][0]["extracted_entry_count"] == entry_count
    assert elapsed < 10.0


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
