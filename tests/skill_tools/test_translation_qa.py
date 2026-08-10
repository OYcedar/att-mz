from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import cast

ROOT = Path(__file__).resolve().parents[2]
QA = ROOT / "skills" / "translate-with-att" / "scripts" / "translation_qa.py"


def run_script(arguments: Sequence[object], *, expected: int = 0) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [sys.executable, str(QA), *(str(argument) for argument in arguments)],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    assert result.returncode == expected, f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    return result


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def write_jsonl(path: Path, values: Sequence[object]) -> None:
    path.write_text(
        "".join(json.dumps(value, ensure_ascii=False) + "\n" for value in values),
        encoding="utf-8",
    )


def write_survey(tmp_path: Path, rows: Sequence[dict[str, object]]) -> Path:
    game = tmp_path / "game"
    (game / "data").mkdir(parents=True)
    (game / "js").mkdir()
    (game / "data" / "System.json").write_text("{}", encoding="utf-8")
    (game / "js" / "plugins.js").write_text("var $plugins = [];", encoding="utf-8")
    survey = tmp_path / "survey"
    survey.mkdir()
    write_json(
        survey / "survey.json",
        {
            "engine": "mv",
            "game_root": str(game),
            "content_root": str(game),
            "locations": len(rows),
            "review_groups": 0,
        },
    )
    write_jsonl(survey / "locations.jsonl", rows)
    (survey / "review-groups.jsonl").write_text("", encoding="utf-8")
    files: list[dict[str, object]] = []
    for relative in ("data/System.json", "js/plugins.js"):
        raw = (game / Path(relative)).read_bytes()
        files.append({"path": relative, "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()})
    write_json(
        survey / "source-baseline.json",
        {
            "scope": "test",
            "files": files,
            "selection": {
                "data_directory": "data",
                "plugins_file": "js/plugins.js",
                "external_suffixes": [".json", ".txt"],
                "paths": ["data/System.json", "js/plugins.js"],
            },
        },
    )
    return survey


def translation_rows(*, corrected: bool) -> list[dict[str, object]]:
    values: list[tuple[str, list[str], list[str] | None, str]] = [
        (
            "Map001.json:event1:page1:command1",
            ["Go to Main %1"],
            ["前往主菜单 %1"] if corrected else ["前往 Main %1"],
            "current",
        ),
        (
            'plugins.js:plugin1:Quest:Objective:"text[0]"',
            ["A long objective"],
            ["短目标"] if corrected else ["这是一个明显超过插件固定窗口宽度且没有换行的中文目标描述"],
            "current",
        ),
        (
            "Map001.json:event1:page1:choices2",
            ["Choice"],
            ["选项"] if corrected else None,
            "current" if corrected else "pending",
        ),
        (
            "Map001.json:event1:page1:choices3",
            ["-Main"],
            ["主线"] if corrected else ["-Main"],
            "current",
        ),
    ]
    return [
        {
            "manual_id": manual_id,
            "source": source,
            "translation": translation,
            "state": state,
            "origin": "automatic" if state == "current" else "none",
            "type": "fixed",
            "owner": "builtin",
        }
        for manual_id, source, translation, state in values
    ]


def test_translation_qa_uses_exports_and_builds_revision_ids(tmp_path: Path) -> None:
    translations = tmp_path / "translations.jsonl"
    rows = translation_rows(corrected=False)
    write_jsonl(translations, rows)
    locations: list[dict[str, object]] = []
    for number, row in enumerate(rows, start=1):
        source_lines = cast(list[str], row["source"])
        locations.append(
            {
                "candidate_id": f"location-{number:06d}",
                "source": "data/System.json",
                "source_text": source_lines[0],
                "classification": "builtin",
                "expected_manual_id": row["manual_id"],
                "roles": ["display"],
                "control_contract": {"consumer": "extended_text"},
            }
        )
    survey = write_survey(tmp_path, locations)
    terms = tmp_path / "terminology.toml"
    terms.write_text("[[term]]\nterm = '-Main'\ntranslation = '主线'\n", encoding="utf-8")
    write_back = tmp_path / "write-back.json"
    write_json(
        write_back,
        {
            "source_unchanged": True,
            "output_json_valid": True,
            "structural_differences": 0,
            "non_text_value_changes": [],
        },
    )
    runtime = tmp_path / "runtime.json"
    write_json(runtime, {"qa_status": "clean"})

    scan = tmp_path / "qa"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--terminology",
            terms,
            "--write-back-preview",
            write_back,
            "--runtime-report",
            runtime,
            "--output",
            scan,
        ]
    )
    report = json.loads((scan / "qa-summary.json").read_text(encoding="utf-8"))
    assert report["qa_status"] == "needs_review"
    assert report["counts"]["extracted_not_translated"] == 1
    assert report["counts"]["layout_risk"] >= 1
    assert report["counts"]["source_residual"] >= 2
    assert report["counts"]["terminology_mismatch"] == 1

    ids = tmp_path / "revision-ids.jsonl"
    run_script(["manual", "--scan", scan, "--output", ids])
    revision_ids = [json.loads(line)["manual_id"] for line in ids.read_text(encoding="utf-8").splitlines()]
    assert revision_ids == [row["manual_id"] for row in rows]

    write_jsonl(translations, translation_rows(corrected=True))
    corrected_scan = tmp_path / "qa-corrected"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--terminology",
            terms,
            "--write-back-preview",
            write_back,
            "--runtime-report",
            runtime,
            "--output",
            corrected_scan,
        ]
    )
    corrected = json.loads((corrected_scan / "qa-summary.json").read_text(encoding="utf-8"))
    assert corrected["qa_status"] == "clean"


def test_static_qa_uses_frozen_survey_but_final_evidence_rechecks_the_game(tmp_path: Path) -> None:
    rows = translation_rows(corrected=True)
    translations = tmp_path / "translations.jsonl"
    write_jsonl(translations, rows)
    locations = [
        {
            "candidate_id": f"location-{number:06d}",
            "source": "data/System.json",
            "source_text": cast(list[str], row["source"])[0],
            "classification": "builtin",
            "expected_manual_id": row["manual_id"],
            "roles": ["display"],
            "control_contract": {"consumer": "extended_text"},
        }
        for number, row in enumerate(rows, start=1)
    ]
    survey = write_survey(tmp_path, locations)
    (tmp_path / "game" / "data" / "System.json").write_text('{"changed":true}', encoding="utf-8")

    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--output",
            tmp_path / "static-qa",
        ]
    )
    write_back = tmp_path / "write-back.json"
    write_json(write_back, {"source_unchanged": True, "output_json_valid": True})
    write_back_result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--write-back-preview",
            write_back,
            "--output",
            tmp_path / "write-back-qa",
        ],
        expected=1,
    )
    assert "来源字节与 scan 时不同" in write_back_result.stderr
    runtime = tmp_path / "runtime.json"
    write_json(runtime, {"qa_status": "clean"})
    runtime_result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--runtime-report",
            runtime,
            "--output",
            tmp_path / "runtime-qa",
        ],
        expected=1,
    )
    assert "来源字节与 scan 时不同" in runtime_result.stderr
