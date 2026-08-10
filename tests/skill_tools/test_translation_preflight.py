from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import cast

ROOT = Path(__file__).resolve().parents[2]
PREFLIGHT = ROOT / "skills" / "translate-with-att" / "scripts" / "translation_preflight.py"


def run_script(arguments: Sequence[object], *, expected: int = 0) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [sys.executable, str(PREFLIGHT), *(str(argument) for argument in arguments)],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    assert result.returncode == expected, f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    return result


def write_manual(path: Path, *, suffix: str = "") -> None:
    path.write_text(
        r"""
[[translation]]
id = 'Map001.json:event1:page1:command1'
type = 'fixed'
source = ['Use %1does and {{name}} \V[1]']
translation = ['']

[[translation]]
id = 'Map001.json:event1:page1:choices2'
type = 'fixed'
source = ['Accept', '', 'Cancel']
translation = ['', '', '']

[[translation]]
id = 'Map001.json:event1:page1:command3'
type = 'free'
source = ['Start \Token[', 'value] {{name}}']
translation = ['', '']

[[translation]]
id = 'Map001.json:event1:page1:dialogue4'
type = 'fixed'
source = ["\u000c"]
translation = ['']
""".lstrip()
        + suffix,
        encoding="utf-8",
    )


def write_survey(tmp_path: Path) -> tuple[Path, Path]:
    game = tmp_path / "game"
    source = game / "www" / "data" / "System.json"
    source.parent.mkdir(parents=True)
    source.write_text("{}", encoding="utf-8")
    raw = source.read_bytes()
    survey = tmp_path / "survey"
    survey.mkdir()
    locations = [
        {
            "candidate_id": "location-000001",
            "source": "data/Map001.json:builtin-events",
            "source_text": r"Use %1does and {{name}} \V[1]",
            "classification": "builtin",
            "expected_manual_id": "Map001.json:event1:page1:command1",
            "manual_type": "fixed",
            "control_contract": {"consumer": "message_text"},
            "review_group_id": "shared-consumer-shape",
        },
        {
            "candidate_id": "location-000002",
            "source": "data/Map001.json:builtin-events",
            "source_text": "Accept\n\nCancel",
            "classification": "builtin",
            "expected_manual_id": "Map001.json:event1:page1:choices2",
            "manual_type": "fixed",
            "control_contract": {"consumer": "extended_text"},
        },
        {
            "candidate_id": "location-000003",
            "source": "data/Map001.json:rules",
            "source_text": "Start \\Token[\nvalue] {{name}}",
            "classification": "review",
            "expected_manual_id": "Map001.json:event1:page1:command3",
            "manual_type": "free",
            "review_group_id": "shared-consumer-shape",
        },
        {
            "candidate_id": "location-000004",
            "source": "data/Map001.json:builtin-events",
            "source_text": "\f",
            "classification": "builtin",
            "expected_manual_id": "Map001.json:event1:page1:dialogue4",
            "manual_type": "fixed",
            "control_contract": {"consumer": "message_text"},
        },
    ]
    (survey / "survey.json").write_text(
        json.dumps(
            {
                "engine": "mz",
                "game_root": str(game),
                "locations": len(locations),
                "review_groups": 0,
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )
    (survey / "locations.jsonl").write_text(
        "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in locations),
        encoding="utf-8",
    )
    (survey / "review-groups.jsonl").write_text("", encoding="utf-8")
    (survey / "source-baseline.json").write_text(
        json.dumps(
            {
                "files": [
                    {
                        "path": "www/data/System.json",
                        "bytes": len(raw),
                        "sha256": hashlib.sha256(raw).hexdigest(),
                    }
                ],
                "selection": {
                    "data_directory": "www/data",
                    "plugins_file": "www/js/plugins.js",
                    "external_suffixes": [".json"],
                    "paths": ["www/data/System.json"],
                },
            }
        ),
        encoding="utf-8",
    )
    coverage = tmp_path / "coverage.json"
    coverage.write_text(
        json.dumps(
            {
                "complete": True,
                "engine": "mz",
                "expected_ownership": [
                    {"manual_id": locations[0]["expected_manual_id"], "owner": "builtin"},
                    {"manual_id": locations[1]["expected_manual_id"], "owner": "builtin"},
                    {"manual_id": locations[2]["expected_manual_id"], "owner": "rules", "rule_number": 1},
                    {"manual_id": locations[3]["expected_manual_id"], "owner": "builtin"},
                ],
                "unit_projection": [
                    {
                        "manual_id": location["expected_manual_id"],
                        "source_text": location["source_text"],
                        "manual_type": location["manual_type"],
                        "control_contract": location.get(
                            "control_contract",
                            {"consumer": "plain_text"},
                        ),
                        "source": location["source"],
                        "candidate_id": location["candidate_id"],
                        **(
                            {"review_group_id": location["review_group_id"]}
                            if "review_group_id" in location
                            else {}
                        ),
                        "owner": "rules" if location is locations[2] else "builtin",
                        **({"rule_number": 1} if location is locations[2] else {}),
                    }
                    for location in locations
                ],
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )
    return survey, coverage


def read_jsonl(path: Path) -> list[dict[str, object]]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def write_jsonl(path: Path, values: Sequence[object]) -> None:
    path.write_text(
        "".join(json.dumps(value, ensure_ascii=False) + "\n" for value in values),
        encoding="utf-8",
    )


def write_single_preflight_case(
    tmp_path: Path,
    *,
    engine: str,
    manual_id: str,
    source_text: str,
    control_contract: dict[str, object],
) -> tuple[Path, Path, Path]:
    manual = tmp_path / "single.toml"
    manual.write_text(
        "[[translation]]\n"
        f"id = {json.dumps(manual_id)}\n"
        "type = 'fixed'\n"
        f"source = [{json.dumps(source_text)}]\n"
        "translation = ['']\n",
        encoding="utf-8",
    )
    game = tmp_path / "single-game"
    source = game / "data" / "System.json"
    source.parent.mkdir(parents=True)
    source.write_text("{}", encoding="utf-8")
    raw = source.read_bytes()
    survey = tmp_path / "single-survey"
    survey.mkdir()
    location = {
        "candidate_id": "location-000001",
        "source": "data/Map001.json:builtin-events",
        "source_text": source_text,
        "classification": "builtin",
        "expected_manual_id": manual_id,
        "manual_type": "fixed",
        "control_contract": control_contract,
    }
    (survey / "survey.json").write_text(
        json.dumps({"engine": engine, "game_root": str(game), "locations": 1, "review_groups": 0}),
        encoding="utf-8",
    )
    write_jsonl(survey / "locations.jsonl", [location])
    (survey / "review-groups.jsonl").write_text("", encoding="utf-8")
    (survey / "source-baseline.json").write_text(
        json.dumps(
            {
                "files": [
                    {
                        "path": "data/System.json",
                        "bytes": len(raw),
                        "sha256": hashlib.sha256(raw).hexdigest(),
                    }
                ],
                "selection": {
                    "data_directory": "data",
                    "plugins_file": "js/plugins.js",
                    "external_suffixes": [".json"],
                    "paths": ["data/System.json"],
                },
            }
        ),
        encoding="utf-8",
    )
    coverage = tmp_path / "single-coverage.json"
    coverage.write_text(
        json.dumps(
            {
                "complete": True,
                "engine": engine,
                "expected_ownership": [{"manual_id": manual_id, "owner": "builtin"}],
                "unit_projection": [
                    {
                        "manual_id": manual_id,
                        "source_text": source_text,
                        "manual_type": "fixed",
                        "control_contract": control_contract,
                        "source": location["source"],
                        "candidate_id": location["candidate_id"],
                        "owner": "builtin",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    return manual, survey, coverage


def test_preflight_uses_survey_coverage_and_records_fixed_slots_without_decisions(tmp_path: Path) -> None:
    manual = tmp_path / "manual.toml"
    write_manual(manual)
    survey, coverage = write_survey(tmp_path)
    output = tmp_path / "preflight"
    first = run_script(["--manual", manual, "--survey", survey, "--coverage", coverage, "--output", output])
    assert "Translate 可运行" in first.stdout
    initial = json.loads((output / "preflight.json").read_text(encoding="utf-8"))
    assert initial["complete"] is False
    assert initial["coverage_complete"] is True
    candidates = read_jsonl(output / "placeholder-candidates.jsonl")
    assert {candidate["kind"] for candidate in candidates} == {
        "placeholder_shape",
        "cross_line_structure",
    }
    assert all(candidate["analysis_status"] == "heuristic_review" for candidate in candidates)
    observed = {
        example["observed_form"]
        for candidate in candidates
        for example in cast(list[dict[str, object]], candidate.get("examples", []))
    }
    assert r"\V[1]" not in observed
    assert "%1" in observed
    mustache = [candidate for candidate in candidates if candidate.get("form") == "mustache"]
    assert len(mustache) == 2
    for candidate in mustache:
        for option in cast(list[dict[str, object]], candidate["rule_options"]):
            rule = cast(dict[str, object], option["rule"])
            assert len(cast(list[object], rule["ids"])) == 1
    fixed = read_jsonl(output / "fixed-structure.jsonl")
    assert fixed == [
        {
            "kind": "fixed_blank_slots",
            "manual_id": "Map001.json:event1:page1:choices2",
            "slot_count": 3,
            "blank_slot_indexes": [1],
            "status": "proven_structure",
        }
    ]
    metrics = json.loads((output / "agent-work-metrics.json").read_text(encoding="utf-8"))
    assert metrics["explicit_decisions_required"] == len(candidates)
    assert metrics["structural_facts"] == 1

    decisions = [
        {
            "target": f"preflight:{candidate['candidate_id']}",
            "decision": "protect" if candidate["kind"] == "placeholder_shape" else "ignore",
            **(
                {
                    "protection": (
                        "format_arguments"
                        if candidate.get("form") == "percent_number"
                        else cast(list[dict[str, object]], candidate["rule_options"])[0]["protection"]
                    )
                }
                if candidate["kind"] == "placeholder_shape" and cast(list[object], candidate["rule_options"])
                else {}
            ),
            "evidence": "已检查活动消费者，确认该外形必须原样保留",
        }
        for candidate in candidates
    ]
    decisions_path = tmp_path / "decisions.jsonl"
    decisions_path.write_text(
        "".join(json.dumps(value, ensure_ascii=False) + "\n" for value in decisions),
        encoding="utf-8",
    )
    run_script(
        [
            "--manual",
            manual,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            output,
            "--decisions",
            decisions_path,
            "--replace",
        ]
    )
    final = json.loads((output / "preflight.json").read_text(encoding="utf-8"))
    assert final["complete"] is True
    rules = (output / "placeholder-rules.toml").read_text(encoding="utf-8")
    assert "pattern = '" in rules
    assert "%[0-9]+" in rules
    assert "order = 'reorder_within_slot'" in rules
    assert "ids = ['Map001.json:event1:page1:command1']" in rules
    assert r"\\V" not in rules
    assert all("pattern" not in decision for decision in decisions)

    write_manual(manual, suffix="\n")
    run_script(
        [
            "--manual",
            manual,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            output,
            "--decisions",
            decisions_path,
            "--replace",
        ],
        expected=1,
    )


def test_preflight_rejects_manual_that_does_not_match_coverage_source(tmp_path: Path) -> None:
    manual = tmp_path / "manual.toml"
    write_manual(manual)
    survey, coverage = write_survey(tmp_path)
    text = manual.read_text(encoding="utf-8").replace("Accept", "Changed", 1)
    manual.write_text(text, encoding="utf-8")
    result = run_script(
        [
            "--manual",
            manual,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            tmp_path / "preflight",
        ],
        expected=1,
    )
    assert "不能映射回 finalize" in result.stderr


def test_mv_namebox_shape_is_reviewed_and_generated_wrapper_keeps_name_translatable(
    tmp_path: Path,
) -> None:
    manual_id = "Map001.json:event1:page1:dialogue1"
    manual, survey, coverage = write_single_preflight_case(
        tmp_path,
        engine="mv",
        manual_id=manual_id,
        source_text=r"\N<Hero\V[1]>Hello",
        control_contract={"consumer": "message_text"},
    )
    output = tmp_path / "preflight-namebox"
    run_script(["--manual", manual, "--survey", survey, "--coverage", coverage, "--output", output])
    candidates = read_jsonl(output / "placeholder-candidates.jsonl")
    namebox = next(candidate for candidate in candidates if candidate["form"] == "backslash_angle")
    assert sum(candidate["form"] == "backslash_angle" for candidate in candidates) == 1
    assert all(candidate["form"] != "angle_tag" for candidate in candidates)
    assert all(candidate["form"] != "backslash_bracket" for candidate in candidates)
    assert namebox["analysis_status"] == "heuristic_review"
    options = cast(list[dict[str, object]], namebox["rule_options"])
    assert [option["protection"] for option in options] == ["shell_with_text"]
    rule = cast(dict[str, object], options[0]["rule"])
    assert rule["ids"] == [manual_id]
    assert "(?P<text>" in str(rule["pattern"])
    assert rule["order"] == "preserve"

    decisions = tmp_path / "namebox-decisions.jsonl"
    write_jsonl(
        decisions,
        [
            {
                "target": f"preflight:{namebox['candidate_id']}",
                "decision": "protect",
                "protection": "shell_with_text",
                "reason": "已确认当前活动姓名框消费者",
                "evidence": "实际插件源码读取该 wrapper 并用 drawTextEx 显示其中姓名",
            }
        ],
    )
    run_script(
        [
            "--manual",
            manual,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            output,
            "--decisions",
            decisions,
            "--replace",
        ]
    )
    rules = (output / "placeholder-rules.toml").read_text(encoding="utf-8")
    assert f"ids = ['{manual_id}']" in rules
    assert "(?P<text>" in rules


def test_wrong_consumer_control_shapes_are_exact_id_reviews(tmp_path: Path) -> None:
    manual_id = "System.json:gameTitle"
    source_text = r"\G \{ \$ \! \FOO \\ " + "\x1bV[2] \x1b!\f"
    manual, survey, coverage = write_single_preflight_case(
        tmp_path,
        engine="mz",
        manual_id=manual_id,
        source_text=source_text,
        control_contract={"consumer": "plain_text"},
    )
    output = tmp_path / "preflight-wrong-consumer"
    run_script(["--manual", manual, "--survey", survey, "--coverage", coverage, "--output", output])
    candidates = read_jsonl(output / "placeholder-candidates.jsonl")
    observed = {
        example["observed_form"]
        for candidate in candidates
        for example in cast(list[dict[str, object]], candidate.get("examples", []))
    }
    assert {r"\G", r"\{", r"\$", r"\!", r"\FOO", r"\\", "\x1bV[2]", "\x1b!", "\f"} <= observed
    assert all(
        cast(dict[str, object], option["rule"])["ids"] == [manual_id]
        for candidate in candidates
        for option in cast(list[dict[str, object]], candidate["rule_options"])
    )


def test_unknown_delimited_commands_require_projection_choice_and_escape_remainders_are_reviewed(
    tmp_path: Path,
) -> None:
    manual_id = "Actors.json:1:profile"
    source_text = r"\Tag[Visible] \? tail" + "\\"
    manual, survey, coverage = write_single_preflight_case(
        tmp_path,
        engine="mz",
        manual_id=manual_id,
        source_text=source_text,
        control_contract={"consumer": "extended_text"},
    )
    output = tmp_path / "preflight-unknown-delimiters"
    run_script(["--manual", manual, "--survey", survey, "--coverage", coverage, "--output", output])
    candidates = read_jsonl(output / "placeholder-candidates.jsonl")
    wrapper = next(candidate for candidate in candidates if candidate["form"] == "backslash_bracket")
    assert [option["protection"] for option in cast(list[dict[str, object]], wrapper["rule_options"])] == [
        "whole_protocol",
        "shell_with_text",
    ]
    remainder_observed = {
        example["observed_form"]
        for candidate in candidates
        if candidate["form"] == "unknown_escape_introducer"
        for example in cast(list[dict[str, object]], candidate["examples"])
    }
    assert {r"\?", "\\"} <= remainder_observed
