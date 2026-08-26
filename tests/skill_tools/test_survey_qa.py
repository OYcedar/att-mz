from __future__ import annotations

import json
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import cast

import pytest

ROOT = Path(__file__).resolve().parents[2]
SURVEY = ROOT / "skills" / "translate-with-att" / "scripts" / "rpg_maker_survey.py"
PREFLIGHT = ROOT / "skills" / "translate-with-att" / "scripts" / "translation_preflight.py"
QA = ROOT / "skills" / "translate-with-att" / "scripts" / "translation_qa.py"


def run_script(
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


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")


def read_jsonl(path: Path) -> list[dict[str, object]]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def write_jsonl(path: Path, values: Sequence[object]) -> None:
    path.write_text(
        "".join(json.dumps(value, ensure_ascii=False) + "\n" for value in values),
        encoding="utf-8",
        newline="\n",
    )


@pytest.fixture
def survey_game(tmp_path: Path) -> Path:
    game = tmp_path / "game"
    data = game / "data"
    scripts = game / "js" / "plugins"
    data.mkdir(parents=True)
    scripts.mkdir(parents=True)
    (game / "Game.exe").write_bytes(b"")
    write_json(game / "package.json", {"name": "Survey Game", "main": "index.html"})
    (game / "index.html").write_text(
        '<script src="js/rpg_core.js"></script><script src="js/direct-custom.js"></script>',
        encoding="utf-8",
    )
    (game / "js" / "direct-custom.js").write_text(
        "window.drawText('Direct HTML visible', 0, 0);",
        encoding="utf-8",
    )
    (game / "js" / "rpg_core.js").write_text("// RPG Maker MV marker.\n", encoding="utf-8")
    plugins = [
        {
            "name": "Options",
            "status": True,
            "description": "",
            "parameters": {
                "Categories": "General, Misc, Sound, Toggles",
                "Default Category": "General",
                "Options": json.dumps(
                    [
                        {"Name": "[General] One", "Category": "General"},
                        {"Name": "Compass Size", "Category": "Misc"},
                    ]
                ),
                "Numeric Internal": "123",
                "Visible Label": "Visible option",
                "Protocol Key": "main-key",
                "Image": "pictures/Hero.png",
                "Decoded Controls": json.dumps({"Label": "Decoded visible"}),
                "One Parse": json.dumps({"Label": "[1,2]"}),
                "Schema Choices": json.dumps(
                    [json.dumps({"Name": "Schema visible", "Key": "schema-key", "Literal": "[3,4]"})]
                ),
                "JSON Lookalike": json.dumps({"Label": "Not consumed"}),
                "Broken Schema": '[{"Name":"Broken"}',
                "Page Break Label": "\f",
            },
        },
        {
            "name": "Disabled",
            "status": False,
            "description": "Disabled plugin",
            "parameters": {"Label": "Hidden label"},
        },
    ]
    (game / "js" / "plugins.js").write_text(
        "var $plugins = " + json.dumps(plugins, ensure_ascii=False) + ";",
        encoding="utf-8",
    )
    (scripts / "Options.js").write_text(
        "/*:\n"
        " * @param Schema Choices\n"
        " * @type struct<SchemaChoice>[]\n"
        " * @param Broken Schema\n"
        " * @type struct<SchemaChoice>[]\n"
        " */\n"
        "/*~struct~SchemaChoice:\n"
        " * @param Name\n"
        " * @type string\n"
        " * @param Key\n"
        " * @type string\n"
        " * @param Literal\n"
        " * @type string\n"
        " */\n"
        "const p = PluginManager.parameters('Options');\n"
        "const file = 'helpers/one.js';\n"
        "window.drawText(p.Categories, 0, 0);\n"
        "window.drawText(p['Visible Label'], 0, 0);\n"
        "window.drawText(JSON.parse(p['Decoded Controls']).Label, 0, 0);\n"
        "window.drawText(JSON.parse(p['One Parse']).Label, 0, 0);\n"
        "function unrelatedConsumer(p) {\n"
        "  window.drawText(JSON.parse(p['JSON Lookalike']).Label, 0, 0);\n"
        "}\n"
        "if (p['Default Category'] === 'General') {}\n"
        "if (p['Protocol Key'] === 'main-key') {}\n"
        "window.drawText('Direct visible', 0, 0);\n"
        "console.log('Debug only');\n",
        encoding="utf-8",
    )
    helpers = scripts / "helpers"
    helpers.mkdir()
    (helpers / "one.js").write_text("const file = 'two.js';", encoding="utf-8")
    (helpers / "two.js").write_text("window.drawText('Recursive visible', 0, 0);", encoding="utf-8")
    write_json(
        data / "System.json",
        {
            "gameTitle": "Game",
            "currencyUnit": "G",
            "terms": {
                "basic": [],
                "commands": [],
                "params": [],
                "messages": {
                    "actorDamage": "%1 takes %2 damage",
                    "expTotal": "Current %1",
                    "customMessage": "Custom %1",
                },
            },
            "elements": [],
            "skillTypes": [],
            "weaponTypes": [],
            "armorTypes": [],
            "equipTypes": [],
        },
    )
    write_json(
        data / "Actors.json",
        [None, {"name": "Hero", "nickname": "", "profile": "", "note": "Actor note"}, None],
    )
    write_json(
        data / "Skills.json",
        [
            None,
            {
                "name": "Skill",
                "description": "Skill help",
                "message1": "%1 uses %2",
                "message2": "%1 follows %2",
            },
        ],
    )
    write_json(
        data / "States.json",
        [
            None,
            {
                "name": "State",
                "message1": "%1 is affected",
                "message2": "%1 enemy state",
                "message3": "%1 remains affected",
                "message4": "%1 recovered",
            },
        ],
    )
    write_json(data / "CommonEvents.json", [None])
    write_json(data / "Troops.json", [None])
    write_json(data / "Extra.json", [None, {"text": "<b>Wrapped</b>"}])
    write_json(
        data / "Map001.json",
        cast(
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
                                    {"code": 401, "parameters": ["\\N1<Hero>Hello"]},
                                    {"code": 102, "parameters": [["-Main", ""], 0, 0, 2, 0]},
                                    {"code": 101, "parameters": []},
                                    {"code": 401, "parameters": ["\f"]},
                                    {"code": 0, "parameters": []},
                                ]
                            }
                        ]
                    },
                ],
            },
        ),
    )
    (game / "patchnotes.txt").write_bytes(b"visible\xfftext")
    (game / "pagebreaks.txt").write_text("before\fafter\n", encoding="utf-8")
    write_json(game / "external.json", {"Page": "\f"})
    return game


def test_survey_keeps_every_fact_and_builds_relation_groups(survey_game: Path, tmp_path: Path) -> None:
    output = tmp_path / "survey"
    run_script(SURVEY, ["scan", "--game", survey_game, "--output", output])
    assert {path.name for path in output.iterdir()} == {
        "survey.json",
        "locations.jsonl",
        "review-groups.jsonl",
        "ownership-decisions.jsonl",
        "source-baseline.json",
        "agent-work-metrics.json",
    }
    summary = json.loads((output / "survey.json").read_text(encoding="utf-8"))
    assert "schema" not in summary
    assert "schema_revision" not in summary
    locations = read_jsonl(output / "locations.jsonl")
    by_text: dict[str, list[dict[str, object]]] = {}
    for item in locations:
        by_text.setdefault(str(item["source_text"]), []).append(item)
    assert any(item["classification"] == "review" for item in by_text["123"])
    assert any(item["classification"] == "structural_whitespace" for item in by_text[""])
    assert any(item["classification"] == "resource_reference" for item in by_text["pictures/Hero.png"])
    assert any(item.get("generic_kind") == "unsupported_encoding" for item in locations)
    ff_locations = by_text["\f"]
    assert any(item.get("expected_manual_id") is not None for item in ff_locations)
    assert all(item["classification"] != "structural_whitespace" for item in ff_locations)
    assert any(item.get("source") == "plugin:Options:parameters" for item in ff_locations)
    assert any(item.get("source") == "external.json" for item in ff_locations)
    assert any(item.get("source") == "pagebreaks.txt" for item in by_text["before\fafter"])
    assert "Recursive visible" in by_text
    assert "Direct HTML visible" in by_text
    decoded_control = next(
        item for item in by_text["Decoded visible"] if item.get("source") == "plugin:Options:parameters"
    )
    assert decoded_control["decode_positions"] == [3]
    assert decoded_control["source_text"] == "Decoded visible"
    assert any(
        evidence.get("basis") == "json_parse"
        for evidence in cast(list[dict[str, object]], decoded_control["consumer_evidence"])
    )
    one_parse_literal = next(
        item
        for item in by_text["[1,2]"]
        if item.get("source") == "plugin:Options:parameters" and "One Parse" in str(item.get("location"))
    )
    assert one_parse_literal["decode_positions"] == [3]
    schema_literal = next(
        item for item in by_text["[3,4]"] if item.get("source") == "plugin:Options:parameters"
    )
    assert schema_literal["decode_positions"] == [3, 4]
    schema_visible = next(
        item for item in by_text["Schema visible"] if item.get("source") == "plugin:Options:parameters"
    )
    assert schema_visible["decode_positions"] == [3, 4]
    assert schema_visible.get("expected_manual_id") is not None
    assert any(
        evidence.get("basis") == "plugin_schema"
        for evidence in cast(list[dict[str, object]], schema_visible["consumer_evidence"])
    )
    json_lookalike = next(
        item
        for item in locations
        if item.get("source") == "plugin:Options:parameters"
        and item.get("source_text") == json.dumps({"Label": "Not consumed"})
    )
    assert json_lookalike["source_text"] == json.dumps({"Label": "Not consumed"})
    assert json_lookalike["decode_positions"] == []
    assert "rule" not in json_lookalike
    assert "expected_manual_id" not in json_lookalike
    assert "Not consumed" not in by_text
    broken_schema = next(
        item
        for item in locations
        if item.get("source") == "plugin:Options:parameters"
        and item.get("source_text") == '[{"Name":"Broken"}'
    )
    assert "rule" not in broken_schema
    assert "expected_manual_id" not in broken_schema
    assert any(
        evidence.get("kind") == "serialized_plugin_parameter_invalid"
        for evidence in cast(list[dict[str, object]], broken_schema["consumer_evidence"])
    )
    actor_damage = next(
        item for item in by_text["%1 takes %2 damage"] if item.get("expected_manual_id") is not None
    )
    assert actor_damage["control_contract"] == {
        "consumer": "extended_text",
        "format_arity": 2,
    }
    exp_total = next(item for item in by_text["Current %1"] if item.get("expected_manual_id") is not None)
    assert exp_total["control_contract"] == {
        "consumer": "plain_text",
        "format_arity": 1,
    }
    custom_message = next(item for item in by_text["Custom %1"] if item.get("expected_manual_id") is not None)
    assert custom_message["control_contract"] == {"consumer": "plain_text"}
    skill_message = next(item for item in by_text["%1 uses %2"] if item.get("expected_manual_id") is not None)
    assert skill_message["control_contract"] == {
        "consumer": "extended_text",
        "format_arity": 1,
    }
    state_message = next(
        item for item in by_text["%1 is affected"] if item.get("expected_manual_id") is not None
    )
    assert state_message["control_contract"] == {"consumer": "message_text"}
    direct = next(
        item for item in by_text["Direct visible"] if item.get("generic_kind") == "javascript_literal"
    )
    debug = next(item for item in by_text["Debug only"] if item.get("generic_kind") == "javascript_literal")
    assert direct["review_group_id"] != debug["review_group_id"]
    visible_parameter = next(
        item for item in by_text["Visible option"] if item.get("source") == "plugin:Options:parameters"
    )
    protocol_parameter = next(
        item for item in by_text["main-key"] if item.get("source") == "plugin:Options:parameters"
    )
    assert visible_parameter["review_group_id"] != protocol_parameter["review_group_id"]
    serialized_options = next(
        item
        for item in locations
        if item.get("source") == "plugin:Options:parameters"
        and str(item.get("location", "")).endswith("Options")
    )
    assert str(serialized_options["source_text"]).startswith('[{"Name": "[General] One"')
    assert "rule" not in serialized_options
    assert "expected_manual_id" not in serialized_options
    assert "[General] One" not in by_text
    groups = read_jsonl(output / "review-groups.jsonl")
    assert [group["group_id"] for group in groups] == [
        f"group-{number:06d}" for number in range(1, len(groups) + 1)
    ]
    decision_template = read_jsonl(output / "ownership-decisions.jsonl")
    assert decision_template == [
        {"target": f"group:{group['group_id']}", "owner": "unresolved"} for group in groups
    ]
    category_ids = {
        str(item["candidate_id"])
        for text in ("General, Misc, Sound, Toggles", "General")
        for item in by_text[text]
        if item["source"] == "plugin:Options:parameters"
    }
    category_group_ids = {
        str(item["review_group_id"]) for item in locations if item.get("candidate_id") in category_ids
    }
    assert len(category_group_ids) == 1
    category_group_id = next(iter(category_group_ids))
    assert (
        next(group for group in groups if group["group_id"] == category_group_id)["kind"] == "relation_group"
    )
    members_path = tmp_path / "category-members.jsonl"
    run_script(
        SURVEY,
        [
            "members",
            "--survey",
            output,
            "--group-id",
            category_group_id,
            "--output",
            members_path,
        ],
    )
    assert read_jsonl(members_path) == [
        item
        for item in locations
        if item.get("classification") == "review" and item.get("review_group_id") == category_group_id
    ]
    run_script(
        SURVEY,
        [
            "members",
            "--survey",
            output,
            "--group-id",
            category_group_id,
            "--output",
            members_path,
        ],
        expected=1,
    )
    run_script(
        SURVEY,
        [
            "members",
            "--survey",
            output,
            "--group-id",
            "group-does-not-exist",
            "--output",
            tmp_path / "unknown-members.jsonl",
        ],
        expected=1,
    )
    assert all(group["kind"] != "mv_dialogue_protocol" for group in groups)
    metrics = json.loads((output / "agent-work-metrics.json").read_text(encoding="utf-8"))
    baseline = json.loads((output / "source-baseline.json").read_text(encoding="utf-8"))
    assert metrics["files_read"] == metrics["file_read_operations"] == len(baseline["files"])


def _decisions_for(groups: Sequence[dict[str, object]]) -> list[dict[str, object]]:
    decisions: list[dict[str, object]] = []
    for group in groups:
        target = f"group:{group['group_id']}"
        uses_rules = group.get("rules_capability") in {"single_shape", "multiple_shapes"}
        if uses_rules:
            decisions.append({"target": target, "owner": "rules"})
        else:
            decisions.append(
                {
                    "target": target,
                    "owner": "exclude",
                    "reason": "测试确认不属于玩家文本",
                    "evidence": "固定测试来源",
                }
            )
    return decisions


def write_projected_manual(
    path: Path,
    coverage: dict[str, object],
) -> None:
    projected = {
        str(item["manual_id"]): item for item in cast(list[dict[str, object]], coverage["unit_projection"])
    }
    chunks: list[str] = []
    for ownership in cast(list[dict[str, object]], coverage["expected_ownership"]):
        manual_id = str(ownership["manual_id"])
        fact = projected[manual_id]
        source_lines = str(fact["source_text"]).split("\n")
        source = ", ".join(json.dumps(line, ensure_ascii=False) for line in source_lines)
        translation = ", ".join("''" for _line in source_lines)
        chunks.append(
            "[[translation]]\n"
            f"id = {json.dumps(manual_id, ensure_ascii=False)}\n"
            f"type = {json.dumps(str(fact['manual_type']))}\n"
            f"source = [{source}]\n"
            f"translation = [{translation}]\n\n"
        )
    path.write_text("".join(chunks), encoding="utf-8")


def test_finalize_uses_target_owner_and_audit_findings_are_nonfatal(
    survey_game: Path, tmp_path: Path
) -> None:
    survey_root = tmp_path / "survey"
    run_script(SURVEY, ["scan", "--game", survey_game, "--output", survey_root])
    groups = read_jsonl(survey_root / "review-groups.jsonl")
    preview_group = next(
        group
        for group in groups
        if group.get("rules_capability") != "none" and cast(list[object], group["examples"])
    )
    preview = cast(list[dict[str, object]], preview_group["examples"])[0]
    assert "source_text" not in preview
    preview["source_text_preview"] = "THIS_PREVIEW_MUST_NOT_BECOME_A_RULE"
    write_jsonl(survey_root / "review-groups.jsonl", groups)
    decisions = tmp_path / "decisions.jsonl"
    write_jsonl(decisions, _decisions_for(groups))
    plan = tmp_path / "plan"
    run_script(
        SURVEY,
        [
            "finalize",
            "--survey",
            survey_root,
            "--decisions",
            decisions,
            "--output",
            plan,
        ],
    )
    coverage = json.loads((plan / "coverage.json").read_text(encoding="utf-8"))
    assert coverage["complete"] is True
    projection = cast(list[dict[str, object]], coverage["unit_projection"])
    dialogue_body = next(item for item in projection if str(item["manual_id"]).endswith(":dialogue1"))
    assert all(not str(item["manual_id"]).endswith(":dialogue1:speaker") for item in projection)
    assert dialogue_body["source_text"] == r"\N1<Hero>Hello"
    assert dialogue_body["manual_type"] == "free"
    dialogue_contract = cast(dict[str, object], dialogue_body["control_contract"])
    assert dialogue_contract["consumer"] == "message_text"
    wrapped = next(item for item in projection if item["source_text"] == "Wrapped")
    assert wrapped["owner"] == "rules"
    wrapped_contract = cast(dict[str, object], wrapped["control_contract"])
    assert wrapped_contract["consumer"] == "plain_text"
    assert "text[0]" in str(wrapped["manual_id"])
    decoded_control = next(item for item in projection if item["source_text"] == "Decoded visible")
    assert decoded_control["owner"] == "rules"
    schema_visible = next(item for item in projection if item["source_text"] == "Schema visible")
    assert schema_visible["owner"] == "rules"
    assert all(item["source_text"] != json.dumps({"Label": "Decoded visible"}) for item in projection)
    serialized_schema = json.dumps([json.dumps({"Name": "Schema visible", "Key": "schema-key"})])
    assert all(item["source_text"] != serialized_schema for item in projection)
    assert all("group_number" not in item for item in coverage["dispositions"])
    manifest = json.loads((plan / "rules-manifest.json").read_text(encoding="utf-8"))["rules"]
    assert "THIS_PREVIEW_MUST_NOT_BECOME_A_RULE" not in json.dumps(manifest)
    assert all(
        {"rule_number", "rule", "candidate_ids", "locations", "expected_manual_ids"} <= set(item)
        for item in manifest
    )
    manual = tmp_path / "manual.toml"
    write_projected_manual(manual, cast(dict[str, object], coverage))
    preflight = tmp_path / "preflight"
    run_script(
        PREFLIGHT,
        [
            "--manual",
            manual,
            "--survey",
            survey_root,
            "--coverage",
            plan / "coverage.json",
            "--output",
            preflight,
        ],
    )
    assert (preflight / "preflight.json").is_file()
    ownership = tmp_path / "ownership.jsonl"
    expected = coverage["expected_ownership"]
    write_jsonl(ownership, expected[:-1])
    report = tmp_path / "audit.json"
    result = run_script(
        SURVEY,
        [
            "audit",
            "--survey",
            survey_root,
            "--plan",
            plan,
            "--ownership",
            ownership,
            "--output",
            report,
        ],
    )
    assert "Translate 可运行" in result.stdout
    audit = json.loads(report.read_text(encoding="utf-8"))
    assert audit["complete"] is False
    assert audit["missing"] == 1

    overlap = tmp_path / "overlap.jsonl"
    first = groups[0]
    write_jsonl(
        overlap,
        [
            {"target": f"group:{first['group_id']}", "owner": "unresolved"},
            {
                "target": f"candidate:{cast(list[str], first['candidate_ids'])[0]}",
                "owner": "unresolved",
            },
        ],
    )
    run_script(
        SURVEY,
        [
            "finalize",
            "--survey",
            survey_root,
            "--decisions",
            overlap,
            "--output",
            tmp_path / "bad-plan",
        ],
        expected=1,
    )


def test_finalize_publishes_att_generic_input_and_qa_uses_the_exact_recipe(
    survey_game: Path, tmp_path: Path
) -> None:
    survey_root = tmp_path / "survey"
    run_script(SURVEY, ["scan", "--game", survey_game, "--output", survey_root])
    locations = read_jsonl(survey_root / "locations.jsonl")
    groups = read_jsonl(survey_root / "review-groups.jsonl")
    selected = next(item for item in locations if item.get("source_text") == "Direct HTML visible")
    selected_id = str(selected["candidate_id"])
    selected_group = str(selected["review_group_id"])
    group_members = [item for item in locations if item.get("review_group_id") == selected_group]
    decisions: list[dict[str, object]] = []
    for group in groups:
        group_id = str(group["group_id"])
        if group_id != selected_group:
            decisions.extend(_decisions_for([group]))
            continue
        for member in group_members:
            candidate_id = str(member["candidate_id"])
            if candidate_id == selected_id:
                decisions.append(
                    {
                        "target": f"candidate:{candidate_id}",
                        "owner": "generic",
                        "generic_evidence": {
                            "exact_location": "活动 HTML 直接加载脚本中的精确字面量",
                            "active_runtime_consumer": "当前行直接调用 drawText",
                            "player_visible_non_image_text": "该值作为绘制正文传入",
                            "builtin_not_owner": "不在 RPG Maker 标准数据来源内",
                            "rules_cannot_map_reversibly": "Rules 不处理活动插件源码字面量",
                            "extract_group_unit_write_back_mapping": "一个字面量对应一个 Generic Unit",
                            "unique_owner": "该位置只由本 Generic 来源处理",
                        },
                    }
                )
            else:
                decisions.append(
                    {
                        "target": f"candidate:{candidate_id}",
                        "owner": "exclude",
                        "reason": "本测试只批准一个精确来源",
                        "evidence": "其余同组成员保持未进入 Generic",
                    }
                )
    decisions_path = tmp_path / "decisions.jsonl"
    write_jsonl(decisions_path, decisions)
    plan = tmp_path / "plan"
    run_script(
        SURVEY,
        [
            "finalize",
            "--survey",
            survey_root,
            "--decisions",
            decisions_path,
            "--output",
            plan,
        ],
    )

    manifest = json.loads((plan / "generic" / "manifest.json").read_text(encoding="utf-8"))
    assert len(manifest["recipes"]) == 1
    recipe = manifest["recipes"][0]
    assert recipe["candidate_id"] == selected_id
    assert recipe["physical_file"] == "js/direct-custom.js"
    assert recipe["kind"] == "javascript_literal"
    assert recipe["manual_id"] == "js/direct-custom.js.jsonl:line1:unit1:text"
    input_path = plan / Path(str(recipe["input_file"]))
    groups_jsonl = read_jsonl(input_path)
    assert groups_jsonl == [
        {
            "id": f"candidate:{selected_id}",
            "kind": "javascript_literal",
            "units": [{"id": selected_id, "text": "Direct HTML visible"}],
        }
    ]

    translations = tmp_path / "generic-translations.jsonl"
    write_jsonl(
        translations,
        [
            {
                "manual_id": recipe["manual_id"],
                "source": [recipe["source"]],
                "translation": ["直接显示的中文"],
                "state": "current",
                "origin": "automatic",
                "type": "free",
            }
        ],
    )
    write_back = tmp_path / "generic-write-back"
    output_path = write_back / "js" / "direct-custom.js.jsonl"
    output_path.parent.mkdir(parents=True)
    write_jsonl(
        output_path,
        [
            {
                "id": f"candidate:{selected_id}",
                "kind": "javascript_literal",
                "units": [{"id": selected_id, "text": "直接显示的中文"}],
            }
        ],
    )
    qa = tmp_path / "qa"
    run_script(
        QA,
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey_root,
            "--coverage",
            plan / "coverage.json",
            "--generic-manifest",
            plan / "generic" / "manifest.json",
            "--write-back",
            write_back,
            "--output",
            qa,
        ],
    )
    summary = json.loads((qa / "qa-summary.json").read_text(encoding="utf-8"))
    assert summary["qa_status"] == "unverified"
    assert summary["unverified"] == [
        "translation_language_pair_unbound",
        "runtime_observation_missing",
        "generic_external_consumption_unverified",
    ]


def test_mv_without_dialogue_group_finalizes_normally(survey_game: Path, tmp_path: Path) -> None:
    map_path = survey_game / "data" / "Map001.json"
    map_root = json.loads(map_path.read_text(encoding="utf-8"))
    map_root["events"] = [None]
    write_json(map_path, map_root)
    survey_root = tmp_path / "survey"
    run_script(SURVEY, ["scan", "--game", survey_game, "--output", survey_root])
    groups = read_jsonl(survey_root / "review-groups.jsonl")
    assert all(group["kind"] != "mv_dialogue_protocol" for group in groups)
    decisions = tmp_path / "decisions.jsonl"
    write_jsonl(decisions, _decisions_for(groups))
    run_script(
        SURVEY,
        [
            "finalize",
            "--survey",
            survey_root,
            "--decisions",
            decisions,
            "--output",
            tmp_path / "plan",
        ],
    )


def test_finalize_rejects_a_new_source_that_was_not_in_the_scan(survey_game: Path, tmp_path: Path) -> None:
    survey_root = tmp_path / "survey"
    run_script(SURVEY, ["scan", "--game", survey_game, "--output", survey_root])
    groups = read_jsonl(survey_root / "review-groups.jsonl")
    decisions = tmp_path / "decisions.jsonl"
    write_jsonl(decisions, _decisions_for(groups))
    (survey_game / "new-source.txt").write_text("New visible source", encoding="utf-8")

    result = run_script(
        SURVEY,
        [
            "finalize",
            "--survey",
            survey_root,
            "--decisions",
            decisions,
            "--output",
            tmp_path / "plan",
        ],
        expected=1,
    )
    assert "来源选择范围与 scan 时不同" in result.stderr


def test_audit_uses_the_frozen_survey_after_finalize(survey_game: Path, tmp_path: Path) -> None:
    survey_root = tmp_path / "survey"
    run_script(SURVEY, ["scan", "--game", survey_game, "--output", survey_root])
    groups = read_jsonl(survey_root / "review-groups.jsonl")
    decisions = tmp_path / "decisions.jsonl"
    write_jsonl(decisions, _decisions_for(groups))
    plan = tmp_path / "plan"
    run_script(
        SURVEY,
        ["finalize", "--survey", survey_root, "--decisions", decisions, "--output", plan],
    )
    coverage = json.loads((plan / "coverage.json").read_text(encoding="utf-8"))
    ownership = tmp_path / "ownership.jsonl"
    write_jsonl(ownership, cast(list[object], coverage["expected_ownership"]))
    (survey_game / "new-source.txt").write_text("New visible source", encoding="utf-8")

    report = tmp_path / "audit.json"
    run_script(
        SURVEY,
        [
            "audit",
            "--survey",
            survey_root,
            "--plan",
            plan,
            "--ownership",
            ownership,
            "--output",
            report,
        ],
    )
    assert json.loads(report.read_text(encoding="utf-8"))["complete"] is True

    survey_summary_path = survey_root / "survey.json"
    survey_summary = json.loads(survey_summary_path.read_text(encoding="utf-8"))
    survey_summary["engine"] = "mz" if coverage["engine"] == "mv" else "mv"
    write_json(survey_summary_path, survey_summary)
    mismatch = run_script(
        SURVEY,
        [
            "audit",
            "--survey",
            survey_root,
            "--plan",
            plan,
            "--ownership",
            ownership,
            "--output",
            tmp_path / "mismatched-audit.json",
        ],
        expected=1,
    )
    assert "survey 引擎与 finalize 计划不一致" in mismatch.stderr


def test_review_grouping_keeps_map_locations_and_finalizes_actual_files(
    survey_game: Path, tmp_path: Path
) -> None:
    first_map = json.loads((survey_game / "data" / "Map001.json").read_text(encoding="utf-8"))
    first_map["events"][1]["name"] = "EV001"
    write_json(survey_game / "data" / "Map001.json", first_map)
    write_json(survey_game / "data" / "Map002.json", first_map)
    survey_root = tmp_path / "survey"
    run_script(SURVEY, ["scan", "--game", survey_game, "--output", survey_root])
    locations = read_jsonl(survey_root / "locations.jsonl")
    map_names = [
        item
        for item in locations
        if item.get("source") in {"data/Map001.json", "data/Map002.json"}
        and cast(dict[str, object], item.get("rule", {})).get("path") == "events[].name"
    ]
    assert {item["source"] for item in map_names} == {
        "data/Map001.json",
        "data/Map002.json",
    }
    assert len({item["review_group_id"] for item in map_names}) == 1

    groups = read_jsonl(survey_root / "review-groups.jsonl")
    incomplete_plan = tmp_path / "incomplete"
    run_script(
        SURVEY,
        [
            "finalize",
            "--survey",
            survey_root,
            "--output",
            incomplete_plan,
        ],
    )
    incomplete = json.loads((incomplete_plan / "coverage.json").read_text(encoding="utf-8"))
    assert incomplete["complete"] is False
    assert incomplete["missing_targets"] == []
    assert len(incomplete["unresolved"]) == len(groups)

    decisions = tmp_path / "decisions.jsonl"
    write_jsonl(decisions, _decisions_for(groups))
    plan = tmp_path / "plan"
    run_script(
        SURVEY,
        [
            "finalize",
            "--survey",
            survey_root,
            "--decisions",
            decisions,
            "--output",
            plan,
        ],
    )
    manifest = json.loads((plan / "rules-manifest.json").read_text(encoding="utf-8"))["rules"]
    map_rule_files = {
        item["rule"]["file"] for item in manifest if item["rule"].get("path") == "events[].name"
    }
    assert map_rule_files == {"Map001.json", "Map002.json"}
