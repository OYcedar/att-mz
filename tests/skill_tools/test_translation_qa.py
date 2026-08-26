from __future__ import annotations

import binascii
import hashlib
import json
import shutil
import struct
import subprocess
import sys
import zlib
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

import pytest

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


def png_bytes(width: int = 80, height: int = 80) -> bytes:
    def chunk(kind: bytes, payload: bytes) -> bytes:
        checksum = binascii.crc32(kind)
        checksum = binascii.crc32(payload, checksum) & 0xFFFFFFFF
        return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", checksum)

    pixels = b"".join(b"\0" + b"\0\0\0\xff" * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(pixels))
        + chunk(b"IEND", b"")
    )


def write_survey(tmp_path: Path, rows: Sequence[Mapping[str, object]]) -> Path:
    normalized_rows = [dict(row) for row in rows]
    for row in normalized_rows:
        if row.get("classification") == "builtin" and isinstance(row.get("expected_manual_id"), str):
            row.setdefault("manual_type", "fixed")
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
    write_jsonl(survey / "locations.jsonl", normalized_rows)
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


def write_coverage(tmp_path: Path, rows: Sequence[dict[str, object]], *, complete: bool = True) -> Path:
    projection: list[dict[str, object]] = []
    for row in rows:
        if row.get("classification") != "builtin" or not isinstance(row.get("expected_manual_id"), str):
            continue
        projection.append(
            {
                "manual_id": row["expected_manual_id"],
                "source_text": row["source_text"],
                "manual_type": "fixed",
                "control_contract": row["control_contract"],
                "source": row["source"],
                "candidate_id": row["candidate_id"],
                "owner": "builtin",
            }
        )
    coverage = tmp_path / "coverage.json"
    write_json(
        coverage,
        {
            "complete": complete,
            "engine": "mv",
            "builtin_candidate_ids": [
                row["candidate_id"] for row in rows if row.get("classification") == "builtin"
            ],
            "resource_reference_candidate_ids": [],
            "structural_whitespace_candidate_ids": [],
            "dispositions": [],
            "unresolved": [] if complete else [{"target": "candidate:unresolved"}],
            "missing_targets": [],
            "expected_ownership": [{"manual_id": row["manual_id"], "owner": "builtin"} for row in projection],
            "unit_projection": projection,
            "counts": {
                "locations": len(rows),
                "review_groups": 0,
                "decisions": 0,
                "rules": 0,
                "generic_groups": 0,
                "unresolved": 0 if complete else 1,
            },
        },
    )
    write_json(tmp_path / "rules-manifest.json", {"rules": []})
    return coverage


def write_rpg_write_back(tmp_path: Path, rows: Sequence[dict[str, object]]) -> Path:
    root = tmp_path / "write_back"
    (root / "www" / "data").mkdir(parents=True)
    (root / "www" / "js").mkdir()
    by_file: dict[str, list[str]] = {}
    for row in rows:
        manual_id = cast(str, row["manual_id"])
        source_name = manual_id.split(":", 1)[0]
        relative = "www/js/plugins.js" if source_name == "plugins.js" else f"www/data/{source_name}"
        value = row["translation"] if row["state"] == "current" else row["source"]
        by_file.setdefault(relative, []).extend(cast(list[str], value))
    for relative, values in by_file.items():
        path = root / Path(relative)
        path.parent.mkdir(parents=True, exist_ok=True)
        if relative.endswith(".json"):
            write_json(path, values)
        else:
            path.write_text(json.dumps(values, ensure_ascii=False), encoding="utf-8")
    return root


def write_runtime_report(tmp_path: Path, write_back: Path) -> Path:
    runtime_game = tmp_path / "runtime-game"
    content = runtime_game / "www"
    content.mkdir(parents=True)
    for source in write_back.rglob("*"):
        if not source.is_file():
            continue
        target = runtime_game / source.relative_to(write_back)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
    (content / "data").mkdir(exist_ok=True)
    (content / "data" / "System.json").write_text("{}", encoding="utf-8")
    (content / "index.html").write_text("<!doctype html>", encoding="utf-8")
    runtime = tmp_path / "runtime"
    runtime.mkdir()
    for name in (
        "draws.jsonl",
        "english-candidates.jsonl",
        "pixel-overflows.jsonl",
        "layout-measurement-unverified.jsonl",
        "runtime-errors.jsonl",
        "font-load-review.jsonl",
    ):
        (runtime / name).write_text("", encoding="utf-8")
    scenario_names = ("title", "new_game", "dialogue", "menu", "quest_log", "options", "save")
    required_hooks = {
        name: True
        for name in (
            "bitmapDrawText",
            "windowDrawText",
            "windowDrawTextEx",
            "addCommand",
            "loadFont",
            "fontManagerLoad",
            "graphicsPrintError",
            "graphicsPrintLoadingError",
        )
    }

    def observer(sequence: int) -> dict[str, object]:
        return {
            "installed": True,
            "hooks": dict(required_hooks),
            "hookRequirements": dict(required_hooks),
            "requiredHooksInstalled": True,
            "pageLoadFinished": True,
            "pollingObserved": True,
            "pollingActive": False,
            "installationFinished": True,
            "sequence": sequence,
            "scene": "Scene_Map",
        }

    scenarios: list[dict[str, object]] = []
    events: list[dict[str, object]] = []
    for number, name in enumerate(scenario_names, start=1):
        screenshot = f"screenshots/{number:02d}-{name}.png"
        screenshot_path = runtime / Path(screenshot)
        screenshot_path.parent.mkdir(parents=True, exist_ok=True)
        screenshot_path.write_bytes(png_bytes())
        scene = {
            "title": "Scene_Title",
            "new_game": "Scene_Map",
            "dialogue": "Scene_Map",
            "menu": "Scene_Menu",
            "quest_log": "Scene_Quest",
            "options": "Scene_Options",
            "save": "Scene_Save",
        }[name]
        context = "Window_Message" if name == "dialogue" else scene
        event: dict[str, object] = {
            "sequence": number,
            "timestampMs": 1_000 + number,
            "kind": "Window_Base.drawTextEx" if name == "dialogue" else "Window_Base.drawText",
            "text": "界面文本",
            "scene": scene,
            "context": context,
            "geometry": {
                "measurementStatus": "measured_plain_text",
                "clippingOverflow": False,
                "overflowLeft": False,
                "overflowRight": False,
                "overflowBottom": False,
            },
            "font": {
                "requestedFontFace": "GameFont",
                "requestedFontSize": 28,
                "requestedFontLoaded": True,
                "glyphFallback": "unverified",
            },
            "observation_scope": {"phase": "scenario", "scenario": name},
        }
        events.append(event)
        scenarios.append(
            {
                "name": name,
                "status": "verified",
                "evidence": f"observed {name}",
                "action": {"supported": True},
                "event_sequence_start": number - 1,
                "event_sequence_end": number,
                "observed_events": 1,
                "observed_draws": 1,
                "screenshot": screenshot,
                "screenshot_width": 80,
                "screenshot_height": 80,
                "observer_start": observer(number - 1),
                "observer_end": observer(number),
            }
        )
    write_jsonl(runtime / "events.jsonl", events)
    write_jsonl(runtime / "draws.jsonl", events)
    report = runtime / "report.json"
    write_json(
        report,
        {
            "qa_status": "unverified",
            "mode": "smoke",
            "engine": "mv",
            "game_root": str(runtime_game),
            "content_root": str(content),
            "owned_pid": 1234,
            "cdp_listener_pid": 1234,
            "page_target": (content / "index.html").as_uri(),
            "input_confirmed_isolated_copy": True,
            "keyboard_injection_used": False,
            "startup": {"status": "ready", "wait_seconds": 0.1},
            "observer": observer(len(scenario_names)),
            "scenarios": scenarios,
            "unverified_scenario_count": 0,
            "event_count": len(events),
            "events_file": "events.jsonl",
            "draw_count": len(scenario_names),
            "draws_file": "draws.jsonl",
            "english_candidate_count": 0,
            "english_candidates_file": "english-candidates.jsonl",
            "pixel_overflow_count": 0,
            "pixel_overflows_file": "pixel-overflows.jsonl",
            "measurement_unverified_count": 0,
            "measurement_unverified_file": "layout-measurement-unverified.jsonl",
            "runtime_error_count": 0,
            "runtime_errors_file": "runtime-errors.jsonl",
            "font_review": {
                "requested_font_not_loaded_count": 0,
                "requested_font_not_loaded_file": "font-load-review.jsonl",
                "glyph_fallback_unverified": True,
                "glyph_fallback_status": "unverified",
            },
        },
    )
    return report


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


def _complete_rpg_qa_inputs(tmp_path: Path) -> tuple[Path, Path, Path, Path, Path]:
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
    coverage = write_coverage(tmp_path, locations)
    write_back = write_rpg_write_back(tmp_path, rows)
    runtime = write_runtime_report(tmp_path, write_back)
    return translations, survey, coverage, write_back, runtime


@pytest.mark.parametrize(
    "corruption",
    [
        "unsupported",
        "zero_draws",
        "fake_png",
        "one_pixel_png",
        "empty_hooks",
        "polling_missing",
        "wrong_subset",
        "wrong_scope",
        "wrong_scene",
        "empty_events",
    ],
)
def test_translation_qa_rejects_unproved_verified_runtime_scenarios(tmp_path: Path, corruption: str) -> None:
    translations, survey, coverage, write_back, runtime = _complete_rpg_qa_inputs(tmp_path)
    report = json.loads(runtime.read_text(encoding="utf-8"))
    scenario = cast(list[dict[str, object]], report["scenarios"])[0]
    if corruption == "unsupported":
        scenario["action"] = {"supported": False, "reason": "not available"}
    elif corruption == "zero_draws":
        scenario["observed_draws"] = 0
    elif corruption == "fake_png":
        screenshot = runtime.parent / cast(str, scenario["screenshot"])
        screenshot.write_bytes(b"\x89PNG\r\n\x1a\n")
    elif corruption == "one_pixel_png":
        screenshot = runtime.parent / cast(str, scenario["screenshot"])
        screenshot.write_bytes(png_bytes(1, 1))
        scenario["screenshot_width"] = 1
        scenario["screenshot_height"] = 1
    elif corruption in {"empty_hooks", "polling_missing"}:
        observer = cast(dict[str, object], scenario["observer_start"])
        if corruption == "empty_hooks":
            observer["hookRequirements"] = {}
        else:
            observer["pollingObserved"] = False
    else:
        events_path = runtime.parent / cast(str, report["events_file"])
        draws_path = runtime.parent / cast(str, report["draws_file"])
        events = [json.loads(line) for line in events_path.read_text(encoding="utf-8").splitlines()]
        if corruption == "wrong_subset":
            write_jsonl(runtime.parent / cast(str, report["english_candidates_file"]), [events[0]])
            report["english_candidate_count"] = 1
        elif corruption == "wrong_scope":
            events[0]["observation_scope"] = {"phase": "scenario", "scenario": "menu"}
            write_jsonl(events_path, events)
            write_jsonl(draws_path, events)
        elif corruption == "empty_events":
            events_path.write_text("", encoding="utf-8")
            draws_path.write_text("", encoding="utf-8")
            report["event_count"] = 0
            report["draw_count"] = 0
        else:
            events[0]["scene"] = "Scene_Map"
            events[0]["context"] = "Window_Map"
            write_jsonl(events_path, events)
            write_jsonl(draws_path, events)
    write_json(runtime, report)
    result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--write-back",
            write_back,
            "--runtime-report",
            runtime,
            "--output",
            tmp_path / "qa",
        ],
        expected=1,
    )
    assert "重新运行" in result.stderr


@pytest.mark.parametrize(
    "disposition",
    [
        {"target": "candidate:review-1", "owner": "builtin", "candidate_ids": ["review-1"]},
        {
            "target": "candidate:review-1",
            "owner": "rules",
            "candidate_ids": ["review-1"],
            "evidence": "producer does not emit this",
        },
        {"target": "candidate:review-1", "owner": "generic", "candidate_ids": ["review-1"]},
        {"target": "candidate:review-1", "owner": "exclude", "candidate_ids": ["review-1"]},
    ],
)
def test_translation_qa_validates_each_coverage_disposition_shape(
    tmp_path: Path, disposition: dict[str, object]
) -> None:
    location: dict[str, object] = {
        "candidate_id": "review-1",
        "source": "js/plugins.js",
        "source_text": "Review me",
        "classification": "review",
        "roles": ["display"],
        "control_contract": {"consumer": "extended_text"},
    }
    survey = write_survey(tmp_path, [location])
    coverage = write_coverage(tmp_path, [location])
    coverage_value = json.loads(coverage.read_text(encoding="utf-8"))
    coverage_value["dispositions"] = [disposition]
    write_json(coverage, coverage_value)
    translations = tmp_path / "translations.jsonl"
    translations.write_text("", encoding="utf-8")
    result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            tmp_path / "qa",
        ],
        expected=1,
    )
    assert "dispositions" in result.stderr


def test_translation_qa_groups_heuristics_and_only_expands_selected_reviews(tmp_path: Path) -> None:
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
    coverage = write_coverage(tmp_path, locations)
    terms = tmp_path / "terminology.toml"
    terms.write_text("[[term]]\nterm = '-Main'\ntranslation = '主线'\n", encoding="utf-8")

    scan = tmp_path / "qa"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--terminology",
            terms,
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
    review_groups = [
        json.loads(line) for line in (scan / "review-groups.jsonl").read_text(encoding="utf-8").splitlines()
    ]
    assert len(review_groups) == report["review_groups"]
    assert report["heuristic_findings"] > len(review_groups)
    assert all(len(group["examples"]) <= 5 for group in review_groups)

    confirmed_ids = tmp_path / "confirmed-ids.jsonl"
    run_script(["manual", "--scan", scan, "--output", confirmed_ids])
    revision_ids = [
        json.loads(line)["manual_id"] for line in confirmed_ids.read_text(encoding="utf-8").splitlines()
    ]
    assert revision_ids == [rows[2]["manual_id"]]

    ids = tmp_path / "revision-ids.jsonl"
    selected_groups = [
        item for group in review_groups for item in ("--review-group", group["review_group_id"])
    ]
    run_script(["manual", "--scan", scan, *selected_groups, "--output", ids])
    revision_ids = [json.loads(line)["manual_id"] for line in ids.read_text(encoding="utf-8").splitlines()]
    assert revision_ids == [row["manual_id"] for row in rows]

    write_jsonl(translations, translation_rows(corrected=True))
    write_back = write_rpg_write_back(tmp_path, translation_rows(corrected=True))
    runtime = write_runtime_report(tmp_path, write_back)
    corrected_scan = tmp_path / "qa-corrected"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--terminology",
            terms,
            "--write-back",
            write_back,
            "--runtime-report",
            runtime,
            "--output",
            corrected_scan,
        ]
    )
    corrected = json.loads((corrected_scan / "qa-summary.json").read_text(encoding="utf-8"))
    assert corrected["qa_status"] == "unverified"
    assert corrected["unverified"] == [
        "translation_language_pair_unbound",
        "rpg_write_back_unit_mapping_unverified",
        "runtime_observation_unverified",
    ]


def test_translation_qa_accepts_rejected_candidates_as_opaque_json_text(tmp_path: Path) -> None:
    translations = tmp_path / "translations.jsonl"
    rows: list[dict[str, object]] = [
        {
            "manual_id": "Map001.json:event1:page1:command1",
            "source": ["Rejected"],
            "translation": None,
            "state": "rejected",
            "origin": "automatic",
            "type": "fixed",
            "owner": "builtin",
            "rejected_candidate_json": '{\n  "translation": ["候选"],\n  "translation": null\n}',
        },
        {
            "manual_id": "Map001.json:event1:page1:command2",
            "source": ["Rejected null"],
            "translation": None,
            "state": "rejected",
            "origin": "automatic",
            "type": "fixed",
            "owner": "builtin",
            "rejected_candidate_json": "null",
        },
    ]
    write_jsonl(translations, rows)
    locations: list[dict[str, object]] = [
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
    coverage = write_coverage(tmp_path, locations)

    scan = tmp_path / "qa"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            scan,
        ]
    )

    assert len(translations.read_text(encoding="utf-8").splitlines()) == 2
    summary = json.loads((scan / "qa-summary.json").read_text(encoding="utf-8"))
    assert summary["qa_status"] == "needs_review"
    assert summary["counts"]["rejected_translation"] == 2
    assert summary["revision_ids"] == [row["manual_id"] for row in rows]


def test_translation_qa_rejects_invalid_state_field_combinations(tmp_path: Path) -> None:
    survey = write_survey(tmp_path, [])
    coverage = write_coverage(tmp_path, [])
    base = {
        "manual_id": "Map001.json:event1:page1:command1",
        "source": ["Source"],
        "translation": None,
        "state": "rejected",
        "origin": "automatic",
        "type": "fixed",
        "owner": "builtin",
        "rejected_candidate_json": "null",
    }
    cases = [
        (
            "missing-candidate",
            {key: value for key, value in base.items() if key != "rejected_candidate_json"},
        ),
        ("rejected-translation", {**base, "translation": ["译文"]}),
        ("pending-candidate", {**base, "state": "pending", "origin": "none"}),
        ("invalid-origin", {**base, "origin": "provider"}),
    ]

    for name, row in cases:
        translations = tmp_path / f"{name}.jsonl"
        write_jsonl(translations, [row])
        result = run_script(
            [
                "scan",
                "--translations",
                translations,
                "--survey",
                survey,
                "--coverage",
                coverage,
                "--output",
                tmp_path / f"qa-{name}",
            ],
            expected=1,
        )
        assert "重新" in result.stderr


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
    coverage = write_coverage(tmp_path, locations)
    write_back = write_rpg_write_back(tmp_path, rows)
    runtime = write_runtime_report(tmp_path, write_back)
    (tmp_path / "game" / "data" / "System.json").write_text('{"changed":true}', encoding="utf-8")

    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            tmp_path / "static-qa",
        ]
    )
    write_back_result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--write-back",
            write_back,
            "--output",
            tmp_path / "write-back-qa",
        ],
        expected=1,
    )
    assert "来源字节与 scan 时不同" in write_back_result.stderr
    runtime_result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--runtime-report",
            runtime,
            "--output",
            tmp_path / "runtime-qa",
        ],
        expected=1,
    )
    assert "来源字节与 scan 时不同" in runtime_result.stderr


def test_clean_static_qa_without_write_back_and_runtime_remains_unverified(tmp_path: Path) -> None:
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
    coverage = write_coverage(tmp_path, locations)
    output = tmp_path / "qa"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            output,
        ]
    )
    summary = json.loads((output / "qa-summary.json").read_text(encoding="utf-8"))
    assert summary["qa_status"] == "unverified"
    assert summary["unverified"] == [
        "translation_language_pair_unbound",
        "write_back_output_missing",
        "runtime_observation_missing",
    ]


def test_source_residual_uses_exact_unicode_sequences_from_each_source(tmp_path: Path) -> None:
    rows: list[dict[str, object]] = [
        {
            "manual_id": f"Map001.json:event1:page1:command{number}",
            "source": [source],
            "translation": [translation],
            "state": "current",
            "origin": "automatic",
            "type": "fixed",
            "owner": "builtin",
        }
        for number, (source, translation) in enumerate(
            (
                ("ゲーム開始", "点击ゲーム开始"),
                ("게임 시작", "点击게임开始"),
                ("セーブ", "保存中文"),
                ("Main", "Mainland"),
            ),
            start=1,
        )
    ]
    translations = tmp_path / "translations.jsonl"
    write_jsonl(translations, rows)
    locations = [
        {
            "candidate_id": f"location-{number:06d}",
            "source": "data/Map001.json",
            "source_text": cast(list[str], row["source"])[0],
            "classification": "builtin",
            "expected_manual_id": row["manual_id"],
            "roles": ["display"],
            "control_contract": {"consumer": "extended_text"},
        }
        for number, row in enumerate(rows, start=1)
    ]
    survey = write_survey(tmp_path, locations)
    coverage = write_coverage(tmp_path, locations)
    output = tmp_path / "qa"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            output,
        ]
    )
    findings = [
        json.loads(line) for line in (output / "findings.jsonl").read_text(encoding="utf-8").splitlines()
    ]
    residual_ids = {finding["manual_id"] for finding in findings if finding["kind"] == "source_residual"}
    assert residual_ids == {rows[0]["manual_id"], rows[1]["manual_id"]}


def test_rules_coverage_must_be_rebuilt_from_survey_and_rules_manifest(tmp_path: Path) -> None:
    rule = {"pattern": "<msg>(?<text>.*?)</msg>"}
    candidate_id = "candidate-rules"
    manual_id = "plugins.js:Demo:Message:text[0]"
    location = {
        "candidate_id": candidate_id,
        "source": "plugin:Demo:parameters",
        "location": "plugins.js:plugin1:Demo:parameters:Message",
        "source_text": "<msg>Hello</msg>",
        "classification": "review",
        "expected_manual_id": manual_id,
        "manual_type": "fixed",
        "roles": ["display_candidate"],
        "control_contract": {"consumer": "plain_text"},
        "rule": rule,
    }
    survey = write_survey(tmp_path, [location])
    write_json(
        tmp_path / "rules-manifest.json",
        {
            "rules": [
                {
                    "rule_number": 1,
                    "rule": rule,
                    "candidate_ids": [candidate_id],
                    "locations": [location["location"]],
                    "expected_manual_ids": [manual_id],
                    "targets": [f"candidate:{candidate_id}"],
                }
            ]
        },
    )
    projection = {
        "manual_id": manual_id,
        "source_text": "Hello",
        "manual_type": "fixed",
        "control_contract": {"consumer": "plain_text"},
        "source": location["source"],
        "candidate_id": candidate_id,
        "owner": "rules",
        "rule_number": 1,
    }
    coverage_value = {
        "complete": True,
        "engine": "mv",
        "builtin_candidate_ids": [],
        "resource_reference_candidate_ids": [],
        "structural_whitespace_candidate_ids": [],
        "dispositions": [
            {
                "target": f"candidate:{candidate_id}",
                "owner": "rules",
                "candidate_ids": [candidate_id],
            }
        ],
        "unresolved": [],
        "missing_targets": [],
        "expected_ownership": [{"manual_id": manual_id, "owner": "rules", "rule_number": 1}],
        "unit_projection": [projection],
        "counts": {
            "locations": 1,
            "review_groups": 0,
            "decisions": 1,
            "rules": 1,
            "generic_groups": 0,
            "unresolved": 0,
        },
    }
    coverage = tmp_path / "coverage.json"
    write_json(coverage, coverage_value)
    translations = tmp_path / "translations.jsonl"
    write_jsonl(
        translations,
        [
            {
                "manual_id": manual_id,
                "source": ["Hello"],
                "translation": ["你好"],
                "state": "current",
                "origin": "automatic",
                "type": "fixed",
                "owner": "rules",
                "rule_number": 1,
            }
        ],
    )
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            tmp_path / "valid-qa",
        ]
    )

    coverage_value["unit_projection"] = []
    coverage_value["expected_ownership"] = []
    write_json(coverage, coverage_value)
    result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            tmp_path / "forged-qa",
        ],
        expected=1,
    )
    assert "不能由 Survey 与 Rules manifest 重建" in result.stderr


def test_generic_write_back_scans_actual_unit_text_for_partial_source_residual(tmp_path: Path) -> None:
    candidate_id = "candidate-generic"
    location = {
        "candidate_id": candidate_id,
        "source": "js/custom.js",
        "location": "js/custom.js:line1:literal1",
        "source_text": "Source Alpha",
        "classification": "review",
        "physical_file": "js/custom.js",
        "roles": ["display_candidate"],
    }
    survey = write_survey(tmp_path, [location])
    evidence = {
        "exact_location": "line1",
        "active_runtime_consumer": "drawText",
        "player_visible_non_image_text": "yes",
        "builtin_not_owner": "yes",
        "rules_cannot_map_reversibly": "yes",
        "extract_group_unit_write_back_mapping": "exact",
        "unique_owner": "generic",
    }
    coverage = tmp_path / "coverage.json"
    write_json(
        coverage,
        {
            "complete": True,
            "engine": "mv",
            "builtin_candidate_ids": [],
            "resource_reference_candidate_ids": [],
            "structural_whitespace_candidate_ids": [],
            "dispositions": [
                {
                    "target": f"candidate:{candidate_id}",
                    "owner": "generic",
                    "candidate_ids": [candidate_id],
                    "evidence": evidence,
                }
            ],
            "unresolved": [],
            "missing_targets": [],
            "expected_ownership": [],
            "unit_projection": [],
            "counts": {
                "locations": 1,
                "review_groups": 0,
                "decisions": 1,
                "rules": 0,
                "generic_groups": 1,
                "unresolved": 0,
            },
        },
    )
    write_json(tmp_path / "rules-manifest.json", {"rules": []})
    manual_id = "js/custom.js.jsonl:line1:unit1:text"
    manifest = tmp_path / "generic-manifest.json"
    write_json(
        manifest,
        {
            "sources": [],
            "decisions": [],
            "recipes": [
                {
                    "manual_id": manual_id,
                    "candidate_id": candidate_id,
                    "source": "Source Alpha",
                    "physical_file": "js/custom.js",
                    "input_file": "generic/input/js/custom.js.jsonl",
                    "group_id": f"candidate:{candidate_id}",
                    "kind": "javascript_literal",
                    "unit_id": candidate_id,
                }
            ],
        },
    )
    translations = tmp_path / "generic-translations.jsonl"
    write_jsonl(
        translations,
        [
            {
                "manual_id": manual_id,
                "source": ["Source Alpha"],
                "translation": ["译文"],
                "state": "current",
                "origin": "automatic",
                "type": "free",
            }
        ],
    )
    write_back = tmp_path / "generic-write-back"
    (write_back / "js").mkdir(parents=True)
    write_jsonl(
        write_back / "js" / "custom.js.jsonl",
        [
            {
                "id": f"candidate:{candidate_id}",
                "kind": "javascript_literal",
                "units": [{"id": candidate_id, "text": "译文 Source"}],
            }
        ],
    )
    output = tmp_path / "generic-qa"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--generic-manifest",
            manifest,
            "--write-back",
            write_back,
            "--output",
            output,
        ]
    )
    findings = [
        json.loads(line) for line in (output / "findings.jsonl").read_text(encoding="utf-8").splitlines()
    ]
    residual = next(item for item in findings if item["kind"] == "write_back_source_residual")
    assert residual["manual_id"] == manual_id
    assert residual["words"] == ["Source"]
    assert residual["introduced_words"] == ["Source"]


def test_rpg_write_back_scans_same_output_file_for_exact_source_residual(tmp_path: Path) -> None:
    translations, survey, coverage, write_back, _runtime = _complete_rpg_qa_inputs(tmp_path)
    output_file = write_back / "www" / "data" / "Map001.json"
    values = cast(list[object], json.loads(output_file.read_text(encoding="utf-8")))
    values.append("Main")
    write_json(output_file, values)
    output = tmp_path / "rpg-residual-qa"
    run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--write-back",
            write_back,
            "--output",
            output,
        ]
    )
    findings = [
        json.loads(line) for line in (output / "findings.jsonl").read_text(encoding="utf-8").splitlines()
    ]
    residuals = [item for item in findings if item["kind"] == "write_back_source_residual"]
    assert residuals
    assert any("Main" in cast(list[str], item["introduced_words"]) for item in residuals)
    assert all(item["scope"] == "same_output_file_without_unit_recipe" for item in residuals)


def test_qa_rejects_same_natural_ids_from_a_different_survey_source(tmp_path: Path) -> None:
    rows = translation_rows(corrected=True)
    translations = tmp_path / "translations.jsonl"
    write_jsonl(translations, rows)
    locations = [
        {
            "candidate_id": f"other-{number:06d}",
            "source": "data/System.json",
            "source_text": f"Different source {number}",
            "classification": "builtin",
            "expected_manual_id": row["manual_id"],
            "roles": ["display"],
            "control_contract": {"consumer": "extended_text"},
        }
        for number, row in enumerate(rows, start=1)
    ]
    other = tmp_path / "other"
    survey = write_survey(other, locations)
    coverage = write_coverage(other, locations)
    result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--output",
            tmp_path / "qa",
        ],
        expected=1,
    )
    assert "与 coverage 的来源" in result.stderr


def test_runtime_clean_label_without_production_evidence_is_rejected(tmp_path: Path) -> None:
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
    coverage = write_coverage(tmp_path, locations)
    fake_runtime = tmp_path / "runtime.json"
    write_json(fake_runtime, {"qa_status": "clean"})
    result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--runtime-report",
            fake_runtime,
            "--output",
            tmp_path / "qa",
        ],
        expected=1,
    )
    assert "运行时报告模式" in result.stderr


def test_runtime_report_must_observe_the_same_write_back_bytes(tmp_path: Path) -> None:
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
    coverage = write_coverage(tmp_path, locations)
    write_back = write_rpg_write_back(tmp_path, rows)
    runtime = write_runtime_report(tmp_path, write_back)
    runtime_report = json.loads(runtime.read_text(encoding="utf-8"))
    deployed = Path(runtime_report["content_root"]) / "data" / "Map001.json"
    deployed.write_text("[]", encoding="utf-8")
    result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--write-back",
            write_back,
            "--runtime-report",
            runtime,
            "--output",
            tmp_path / "qa",
        ],
        expected=1,
    )
    assert "与实际 write_back 不一致" in result.stderr


def test_runtime_copy_must_retain_unwritten_survey_source_bytes(tmp_path: Path) -> None:
    translations, survey, coverage, write_back, runtime = _complete_rpg_qa_inputs(tmp_path)
    report = json.loads(runtime.read_text(encoding="utf-8"))
    unchanged_source = Path(report["content_root"]) / "data" / "System.json"
    unchanged_source.write_text('{"from":"another-game"}', encoding="utf-8")
    result = run_script(
        [
            "scan",
            "--translations",
            translations,
            "--survey",
            survey,
            "--coverage",
            coverage,
            "--write-back",
            write_back,
            "--runtime-report",
            runtime,
            "--output",
            tmp_path / "qa",
        ],
        expected=1,
    )
    assert "未写回来源与 Survey 基线不一致" in result.stderr
