from __future__ import annotations

import argparse
import contextlib
import io
import json
import re
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

import summarize_att_run
import translation_preflight
import translation_qa
from att_skill_tools import ManualEntry, ToolError
from att_toolbox.rpg_control_codes import ControlContract, builtin_control_spans
from att_toolbox.translation_export import read_translation_export


def _translation_row(
    source: list[str],
    translation: list[str],
    *,
    unit_type: str = "free",
) -> dict[str, object]:
    return {
        "manual_id": "Map001.json:event1:page1:command1:dialogue",
        "source": source,
        "translation": translation,
        "state": "current",
        "origin": "manual",
        "type": unit_type,
        "owner": "builtin",
        "rule_number": None,
    }


def _term(term: str, translation: str, *triggers: str) -> translation_qa._TerminologyEntry:
    return translation_qa._TerminologyEntry(term, translation, triggers or (term,))


def _log_record(sequence: int, event: str, payload: object, *, level: str = "info") -> dict[str, object]:
    return {
        "timestamp": f"2026-01-01T00:00:{sequence:02d}Z",
        "sequence": sequence,
        "run_id": "run-000001",
        "level": level,
        "event": event,
        "context": {"locale": "zh-Hans", "engine": "generic", "project": "test", "command": "translate"},
        "payload": payload,
        "message": event,
    }


def _generic_complete_result() -> dict[str, object]:
    return {
        "result": {
            "kind": "complete",
            "tasks": {
                "planned": 1,
                "started": 1,
                "complete": 1,
                "partial": 0,
                "unavailable": 0,
                "failed": 0,
                "cancelled": 0,
                "not_started": 0,
            },
            "summary": {
                "engine": "generic",
                "summary": {
                    "planned_units": 1,
                    "remaining_units": 0,
                    "rejected_units": 0,
                    "cleared_units": 0,
                    "reused_units": 0,
                    "accepted_units": 1,
                    "written_units": 1,
                    "conflicted_units": 0,
                    "response_problems": 0,
                    "recoverable_request_exhaustions": 0,
                    "request_admission_stopped": False,
                },
            },
        }
    }


class PlaceholderAndQaFixTests(unittest.TestCase):
    def test_builtin_control_spans_include_bare_commands_without_prefix_capture(self) -> None:
        text = r"\CENTER[1] \C \I \FSIZE[20] \FS \PX \PY"
        mv = [
            text[start:end]
            for start, end, _kind in builtin_control_spans("mv", text, ControlContract("extended_text"))
        ]
        mz = [
            text[start:end]
            for start, end, _kind in builtin_control_spans("mz", text, ControlContract("extended_text"))
        ]

        self.assertEqual(mv, [r"\C", r"\I"])
        self.assertEqual(mz, [r"\C", r"\I", r"\FS", r"\PX", r"\PY"])

    def test_preflight_offers_text_captures_for_angle_wrappers(self) -> None:
        entries = [
            ManualEntry(
                readable_id="id",
                translation_type="free",
                source=("<Help:English text> <msg>Hello</msg>",),
                translation=(),
            )
        ]
        candidates, _fixed, _facts = translation_preflight._scan(
            "mz",
            entries,
            {"id": "builtin"},
            {
                "id": {
                    "control_contract": {"consumer": "plain_text"},
                    "content_kind": "value",
                }
            },
        )
        by_form = {candidate.get("form"): candidate for candidate in candidates}

        for form in ("angle_label", "paired_angle_tag"):
            options = by_form[form]["rule_options"]
            self.assertTrue(
                any(
                    isinstance(option, dict)
                    and option.get("protection") == "shell_with_text"
                    and "(?P<text>" in str(option.get("rule"))
                    for option in options
                )
            )

    def test_preflight_keeps_paired_angle_protection_within_physical_source_slots(self) -> None:
        entry = ManualEntry(
            readable_id="id",
            translation_type="free",
            source=("<msg>第一行", "第二行</msg>"),
            translation=(),
        )
        candidates, _fixed, _facts = translation_preflight._scan(
            "mz",
            [entry],
            {"id": "builtin"},
            {
                "id": {
                    "control_contract": {"consumer": "plain_text"},
                    "content_kind": "lines",
                }
            },
        )
        paired = next(candidate for candidate in candidates if candidate.get("form") == "paired_angle_tag")
        options = paired["rule_options"]
        self.assertEqual(
            [option["protection"] for option in options if isinstance(option, dict)],
            ["shell_with_text"],
        )
        shell_rule = next(
            option["rule"]
            for option in options
            if isinstance(option, dict) and option.get("protection") == "shell_with_text"
        )
        translation_preflight._verify_generated_rules([shell_rule], [entry], {"id": "lines"})

        whole_rule = {
            "ids": ["id"],
            "order": "preserve",
            "pattern": re.escape("\n".join(entry.source)),
        }
        with self.assertRaises(ToolError):
            translation_preflight._verify_generated_rules([whole_rule], [entry], {"id": "lines"})

        value_candidates, _fixed, _facts = translation_preflight._scan(
            "mz",
            [entry],
            {"id": "builtin"},
            {
                "id": {
                    "control_contract": {"consumer": "plain_text"},
                    "content_kind": "value",
                }
            },
        )
        value_paired = next(
            candidate for candidate in value_candidates if candidate.get("form") == "paired_angle_tag"
        )
        value_whole_rule = next(
            option["rule"]
            for option in value_paired["rule_options"]
            if isinstance(option, dict) and option.get("protection") == "whole_protocol"
        )
        translation_preflight._verify_generated_rules(
            [value_whole_rule],
            [entry],
            {"id": "value"},
        )

        crossing_shell_entry = ManualEntry(
            readable_id="crossing-shell",
            translation_type="free",
            source=("<msg", "正文</msg>"),
            translation=(),
        )
        crossing_shell_rule = {
            "ids": ["crossing-shell"],
            "order": "preserve",
            "pattern": re.escape("<msg\n") + r"(?P<text>.*?)" + re.escape("</msg>"),
        }
        with self.assertRaises(ToolError):
            translation_preflight._verify_generated_rules(
                [crossing_shell_rule],
                [crossing_shell_entry],
                {"crossing-shell": "lines"},
            )

        malformed_opening = ManualEntry(
            readable_id="malformed-opening",
            translation_type="free",
            source=("<msg", " class=x>正文</msg>"),
            translation=(),
        )
        malformed_candidates, _fixed, _facts = translation_preflight._scan(
            "mz",
            [malformed_opening],
            {"malformed-opening": "builtin"},
            {
                "malformed-opening": {
                    "control_contract": {"consumer": "plain_text"},
                    "content_kind": "lines",
                }
            },
        )
        self.assertNotIn("paired_angle_tag", {candidate.get("form") for candidate in malformed_candidates})

    def test_free_units_compare_controls_after_reflow_and_scan_added_lines(self) -> None:
        reflow = translation_qa._translation_findings(
            _translation_row([r"\C[2]原文", "后句"], ["译文", r"\C[2]后文"]),
            [],
        )
        self.assertNotIn("control_shape_review", {finding["kind"] for finding in reflow})

        added = translation_qa._translation_findings(
            _translation_row(["原文"], ["译文", r"This is a model explanation \C[9]"]),
            [],
        )
        self.assertIn("control_shape_review", {finding["kind"] for finding in added})

        explanation = translation_qa._translation_findings(
            _translation_row(["原文"], ["译文", "Translation complete."]),
            [],
        )
        self.assertIn("model_explanation_review", {finding["kind"] for finding in explanation})

        mixed_cues = translation_qa._translation_findings(
            _translation_row(["Note: 原文说明"], ["译文", "Translation complete."]),
            [],
        )
        self.assertIn("model_explanation_review", {finding["kind"] for finding in mixed_cues})

    def test_free_units_preserve_control_order_across_lines(self) -> None:
        findings = translation_qa._translation_findings(
            _translation_row([r"\C[1]原文", r"后文\I[2]"], [r"\I[2]译文", r"后文\C[1]"]),
            [],
        )
        self.assertIn("control_shape_review", {finding["kind"] for finding in findings})

    def test_wrapper_payload_remains_visible_to_residual_review(self) -> None:
        findings = translation_qa._translation_findings(
            _translation_row(["<Help:EnglishText>"], ["<Help:EnglishText>"]),
            [],
        )
        self.assertIn("source_residual", {finding["kind"] for finding in findings})

        translated = translation_qa._translation_findings(
            _translation_row([r"\Name[EnglishText]"], [r"\Name[中文]"]),
            [],
        )
        self.assertNotIn("control_shape_review", {finding["kind"] for finding in translated})
        self.assertEqual(
            translation_qa._visible_text(
                '<a href="https://example.com"><span style="color:red">正文</span></a>'
            ),
            "正文",
        )

    def test_terminology_uses_longest_match_at_same_start(self) -> None:
        findings = translation_qa._translation_findings(
            _translation_row(["魔法剣"], ["魔法剑"]),
            [_term("剣", "宝剑"), _term("魔法剣", "魔法剑")],
        )
        self.assertNotIn("terminology_mismatch", {finding["kind"] for finding in findings})

        wrapped = translation_qa._translation_findings(
            _translation_row([r"\Name[Harold] <Help:魔法剣>"], [r"\Name[哈罗德] <Help:魔法剑>"]),
            [_term("Harold", "哈罗德")],
        )
        self.assertNotIn("terminology_mismatch", {finding["kind"] for finding in wrapped})
        wrapped_longest = translation_qa._translation_findings(
            _translation_row(["<Help:魔法剣>"], ["<Help:魔法剑>"]),
            [_term("剣", "宝剑"), _term("魔法剣", "魔法剑")],
        )
        self.assertNotIn("terminology_mismatch", {finding["kind"] for finding in wrapped_longest})
        wrong_wrapped = translation_qa._translation_findings(
            _translation_row(["<Help:魔法剣>"], ["<Help:别的>"]),
            [_term("剣", "宝剑"), _term("魔法剣", "魔法剑")],
        )
        self.assertIn("terminology_mismatch", {finding["kind"] for finding in wrong_wrapped})

    def test_terminology_deduplicates_entries_and_ignores_opaque_or_cross_boundary_text(self) -> None:
        repeated = translation_qa._translation_findings(
            _translation_row(["Alice met Alice and Alicia"], ["错误"]),
            [_term("Alice", "爱丽丝", "Alice", "Alicia")],
        )
        mismatches = [finding for finding in repeated if finding["kind"] == "terminology_mismatch"]
        self.assertEqual(len(mismatches), 1)

        opaque_target = translation_qa._translation_findings(
            _translation_row(["Foo"], ["<span>错误</span>"]),
            [_term("Foo", "span")],
        )
        self.assertIn("terminology_mismatch", {finding["kind"] for finding in opaque_target})

        split_source = translation_qa._translation_findings(
            _translation_row([r"魔\C[1]法"], [r"错\C[1]误"]),
            [_term("魔法", "魔法")],
        )
        self.assertNotIn("terminology_mismatch", {finding["kind"] for finding in split_source})


class ArtifactQaFixTests(unittest.TestCase):
    def test_invalid_preflight_and_runtime_enum_types_have_tool_errors(self) -> None:
        with self.assertRaises(ToolError):
            translation_preflight._control_contract({"control_contract": {"consumer": []}})
        with self.assertRaises(ToolError):
            translation_qa._runtime_scope(
                {"observation_scope": {"phase": [], "scenario": None}},
                "runtime event",
            )

    def test_standalone_generic_recipe_uses_group_kind(self) -> None:
        unit = translation_qa._GenericTreeUnit(
            manual_id="sample.jsonl:line1:unit1:text",
            relative_path="sample.jsonl",
            group_id="group-1",
            kind="entry",
            unit_id="unit-1",
            text="第一行\n第二行",
        )
        tree = translation_qa._GenericTree(
            root=Path("D:/generic"),
            files=("sample.jsonl",),
            units=(unit,),
            fact={},
        )

        recipe = translation_qa._standalone_generic_recipes(tree)[unit.manual_id]

        self.assertEqual(recipe["group_kind"], "entry")
        self.assertNotIn("kind", recipe)
        translation_qa._bind_generic_export_to_manifest(
            [
                {
                    **_translation_row(["第一行", "第二行"], ["译文"]),
                    "manual_id": unit.manual_id,
                    "owner": None,
                }
            ],
            {unit.manual_id: recipe},
        )

    def test_generic_write_back_allows_current_layout_transform_but_preserves_unaccepted_source(self) -> None:
        manual_id = "sample.jsonl:line1:unit1:text"
        manifest = {
            manual_id: {
                "input_file": "generic/input/sample.jsonl",
                "group_id": "group-1",
                "group_kind": "entry",
                "unit_id": "unit-1",
            }
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "sample.jsonl"

            def write_output(text: str) -> None:
                output.write_text(
                    json.dumps(
                        {"id": "group-1", "kind": "entry", "units": [{"id": "unit-1", "text": text}]},
                        ensure_ascii=False,
                    )
                    + "\n",
                    encoding="utf-8",
                    newline="",
                )

            current = {
                **_translation_row(["原文"], ["第一行", "第二行"]),
                "manual_id": manual_id,
                "owner": None,
            }
            write_output("第一行\n  第二行")
            _findings, transformed, _fact = translation_qa._generic_write_back_findings(
                root,
                [current],
                manifest,
                expected_files={"sample.jsonl"},
            )
            self.assertTrue(transformed)

            pending = {
                **current,
                "state": "pending",
                "translation": None,
            }
            write_output("改动")
            with self.assertRaises(ToolError):
                translation_qa._generic_write_back_findings(
                    root,
                    [pending],
                    manifest,
                    expected_files={"sample.jsonl"},
                )

            symbol_current = {
                **_translation_row([r"A+B=C\X"], ["甲＋乙＝丙＼X"]),
                "manual_id": manual_id,
                "owner": None,
            }
            write_output(r"甲+乙=丙\X")
            _findings, symbol_transformed, _fact = translation_qa._generic_write_back_findings(
                root,
                [symbol_current],
                manifest,
                expected_files={"sample.jsonl"},
            )
            self.assertTrue(symbol_transformed)

            for expected, damaged in (
                (r"译文\N[1]", "译文N1"),
                ("<x>译文", "x译文"),
            ):
                protected = {
                    **_translation_row(["原文"], [expected]),
                    "manual_id": manual_id,
                    "owner": None,
                }
                write_output(damaged)
                with self.subTest(expected=expected), self.assertRaises(ToolError):
                    translation_qa._generic_write_back_findings(
                        root,
                        [protected],
                        manifest,
                        expected_files={"sample.jsonl"},
                    )

    def test_manual_output_cannot_enter_survey_game_root(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            game = root / "game"
            scan = root / "qa"
            game.mkdir()
            scan.mkdir()
            translation = root / "translation.jsonl"
            translation.write_text("", encoding="utf-8")
            summary = {
                "translation_export": translation_qa._file_fact(
                    translation,
                    "ATT Translation export JSONL",
                ),
                "revision_ids": [],
                "coverage": {},
                "survey_game_root": str(game),
            }
            (scan / "qa-summary.json").write_text(json.dumps(summary), encoding="utf-8")

            with self.assertRaises(ToolError):
                translation_qa._manual(
                    argparse.Namespace(
                        scan=scan,
                        review_group=[],
                        output=game / "manual-ids.jsonl",
                        replace=True,
                    )
                )


class JsonlAndLogFixTests(unittest.TestCase):
    def test_translation_export_preserves_unicode_line_separator(self) -> None:
        row = _translation_row(["before\u2028after"], ["译文"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "translation.jsonl"
            path.write_text(json.dumps(row, ensure_ascii=False) + "\n", encoding="utf-8")
            self.assertEqual(read_translation_export(path)[0]["source"], ["before\u2028after"])

    def test_translation_export_rejects_a_physical_lone_cr(self) -> None:
        row = _translation_row(["source"], ["译文"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "translation.jsonl"
            path.write_bytes(json.dumps(row, ensure_ascii=False).encode("utf-8") + b"\r")

            with self.assertRaises(ToolError):
                read_translation_export(path)

    def test_bad_export_enum_type_is_a_field_error(self) -> None:
        row = _translation_row(["source"], ["译文"])
        row["state"] = []
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "translation.jsonl"
            path.write_text(json.dumps(row) + "\n", encoding="utf-8")
            with self.assertRaises(ToolError) as caught:
                read_translation_export(path)
            self.assertIn("state", caught.exception.reason)

    def test_summarizer_accepts_current_provider_and_rejected_fields(self) -> None:
        records = [
            _log_record(1, "run.started", {}),
            _log_record(2, "task.started", {"task": {"ordinal": 1, "total": 1}}),
            _log_record(
                3,
                "task.finished",
                {
                    "task": {"ordinal": 1, "total": 1},
                    "attempts": 1,
                    "provider": None,
                    "outcome": {"kind": "complete"},
                },
            ),
            _log_record(
                4,
                "translation.finished",
                {
                    "result": {
                        "kind": "complete",
                        "tasks": {
                            "planned": 1,
                            "started": 1,
                            "complete": 1,
                            "partial": 0,
                            "unavailable": 0,
                            "failed": 0,
                            "cancelled": 0,
                            "not_started": 0,
                        },
                        "summary": {
                            "engine": "generic",
                            "summary": {
                                "planned_units": 1,
                                "remaining_units": 0,
                                "rejected_units": 0,
                                "cleared_units": 0,
                                "reused_units": 0,
                                "accepted_units": 1,
                                "written_units": 1,
                                "conflicted_units": 0,
                                "response_problems": 0,
                                "recoverable_request_exhaustions": 0,
                                "request_admission_stopped": False,
                            },
                        },
                    }
                },
            ),
            _log_record(5, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run-000001.jsonl"
            path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
            result = summarize_att_run._summarize_one(path)
            self.assertEqual(result["task_outcomes"], {"complete": 1})

        rpg_summary = summarize_att_run._translation_summary(
            {
                "engine": "rpg_maker",
                "summary": {
                    "accepted_decisions": 1,
                    "written_locations": 1,
                    "remaining_decisions": 1,
                    "remaining_locations": 2,
                    "rejected_locations": 1,
                    "protocol_diagnostics": 0,
                    "recoverable_request_exhaustions": 0,
                    "request_admission_stopped": False,
                    "retained": 0,
                    "invalidated": 0,
                    "not_applicable": 0,
                    "reused": 0,
                },
            },
            "translation summary",
        )
        self.assertEqual(rpg_summary["rejected_locations"], 1)

    def test_summarizer_accepts_dropped_best_effort_task_started(self) -> None:
        records = [
            _log_record(1, "run.started", {}),
            _log_record(
                2,
                "task.finished",
                {
                    "task": {"ordinal": 1, "total": 1},
                    "attempts": 1,
                    "provider": None,
                    "outcome": {"kind": "complete"},
                },
            ),
            _log_record(
                3,
                "translation.finished",
                {
                    "result": {
                        "kind": "complete",
                        "tasks": {
                            "planned": 1,
                            "started": 1,
                            "complete": 1,
                            "partial": 0,
                            "unavailable": 0,
                            "failed": 0,
                            "cancelled": 0,
                            "not_started": 0,
                        },
                        "summary": {
                            "engine": "generic",
                            "summary": {
                                "planned_units": 1,
                                "remaining_units": 0,
                                "rejected_units": 0,
                                "cleared_units": 0,
                                "reused_units": 0,
                                "accepted_units": 1,
                                "written_units": 1,
                                "conflicted_units": 0,
                                "response_problems": 0,
                                "recoverable_request_exhaustions": 0,
                                "request_admission_stopped": False,
                            },
                        },
                    }
                },
            ),
            _log_record(4, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run-000001.jsonl"
            path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
            result = summarize_att_run._summarize_one(path)
            self.assertEqual(result["task_outcomes"], {"complete": 1})

    def test_summarizer_rejects_task_events_outside_lifecycle_order(self) -> None:
        finished_payload = {
            "task": {"ordinal": 1, "total": 1},
            "attempts": 1,
            "provider": None,
            "outcome": {"kind": "complete"},
        }
        start_after_finish = [
            _log_record(1, "run.started", {}),
            _log_record(2, "task.finished", finished_payload),
            _log_record(3, "task.started", {"task": {"ordinal": 1, "total": 1}}),
            _log_record(4, "translation.finished", _generic_complete_result()),
            _log_record(5, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        task_after_terminal = [
            _log_record(1, "run.started", {}),
            _log_record(2, "translation.finished", {"result": {"kind": "not_started"}}, level="warn"),
            _log_record(3, "task.finished", finished_payload),
            _log_record(4, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, records in (
                ("start-after-finish", start_after_finish),
                ("task-after-terminal", task_after_terminal),
            ):
                path = root / f"{name}.jsonl"
                path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
                with self.subTest(name=name), self.assertRaises(ToolError):
                    summarize_att_run._summarize_one(path)

    def test_summarizer_rejects_impossible_project_log_transitions(self) -> None:
        plan = {
            "plan": {
                "kind": "translate",
                "source": "explicit",
                "profile": "default",
                "terminology": None,
                "placeholders": None,
            }
        }
        finalized = {
            "database": "D:/project.sqlite3",
            "result": {"kind": "saved", "transaction": "committed", "run_continues": True},
        }
        translation = {"result": {"kind": "not_started"}}
        cases = {
            "finalized-without-resolved": [
                _log_record(1, "run.started", {}),
                _log_record(2, "run_plan.finalized", finalized),
                _log_record(3, "translation.finished", translation, level="warn"),
                _log_record(4, "run.finished", {"result": {"kind": "cancelled"}}),
            ],
            "duplicate-plan": [
                _log_record(1, "run.started", {}),
                _log_record(2, "run_plan.resolved", plan),
                _log_record(3, "run_plan.resolved", plan),
                _log_record(4, "translation.finished", translation, level="warn"),
                _log_record(5, "run.finished", {"result": {"kind": "cancelled"}}),
            ],
            "plan-command-mismatch": [
                _log_record(1, "run.started", {}),
                _log_record(
                    2,
                    "run_plan.resolved",
                    {"plan": {"kind": "init", "source": "explicit", "game_root": "D:/game"}},
                ),
                _log_record(3, "translation.finished", translation, level="warn"),
                _log_record(4, "run.finished", {"result": {"kind": "cancelled"}}),
            ],
            "successful-unfinalized-plan": [
                _log_record(1, "run.started", {}),
                _log_record(2, "run_plan.resolved", plan),
                _log_record(3, "translation.finished", translation, level="warn"),
                _log_record(4, "run.finished", {"result": {"kind": "succeeded"}}),
            ],
            "publication-finish-without-start": [
                _log_record(1, "run.started", {}),
                _log_record(
                    2,
                    "publication.finished",
                    {"result": {"kind": "not_published"}},
                    level="error",
                ),
                _log_record(3, "translation.finished", translation, level="warn"),
                _log_record(4, "run.finished", {"result": {"kind": "cancelled"}}),
            ],
            "duplicate-publication": [
                _log_record(1, "run.started", {}),
                _log_record(2, "publication.started", {"output_root": "D:/output"}),
                _log_record(3, "publication.started", {"output_root": "D:/output"}),
                _log_record(4, "translation.finished", translation, level="warn"),
                _log_record(5, "run.finished", {"result": {"kind": "cancelled"}}),
            ],
            "unfinished-publication": [
                _log_record(1, "run.started", {}),
                _log_record(2, "publication.started", {"output_root": "D:/output"}),
                _log_record(3, "translation.finished", translation, level="warn"),
                _log_record(4, "run.finished", {"result": {"kind": "cancelled"}}),
            ],
            "duplicate-phase-terminal": [
                _log_record(
                    1,
                    "run.started",
                    {},
                ),
                _log_record(
                    2,
                    "phase.completed",
                    {"phase": "planning", "amount": {"kind": "indeterminate"}},
                ),
                _log_record(
                    3,
                    "phase.stopped",
                    {"phase": "planning", "outcome": {"kind": "cancelled"}},
                    level="warn",
                ),
                _log_record(4, "translation.finished", translation, level="warn"),
                _log_record(5, "run.finished", {"result": {"kind": "cancelled"}}),
            ],
            "duplicate-cancellation": [
                _log_record(1, "run.started", {}),
                _log_record(2, "run.cancel_requested", {"confirmed": 0, "total": None}, level="warn"),
                _log_record(3, "run.cancel_requested", {"confirmed": 0, "total": None}, level="warn"),
                _log_record(4, "translation.finished", translation, level="warn"),
                _log_record(5, "run.finished", {"result": {"kind": "cancelled"}}),
            ],
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, records in cases.items():
                path = root / f"{name}.jsonl"
                path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
                with self.subTest(name=name), self.assertRaises(ToolError):
                    summarize_att_run._summarize_one(path)

    def test_summarizer_uses_current_safe_text_contract_for_payloads(self) -> None:
        translation = _log_record(
            3,
            "translation.finished",
            {"result": {"kind": "not_started"}},
            level="warn",
        )
        finished = _log_record(4, "run.finished", {"result": {"kind": "cancelled"}})
        unsafe_cases = {
            "profile": _log_record(
                2,
                "run_plan.resolved",
                {
                    "plan": {
                        "kind": "translate",
                        "source": "explicit",
                        "profile": "bad\nprofile",
                        "terminology": None,
                        "placeholders": None,
                    }
                },
            ),
            "publication-path": _log_record(
                2,
                "publication.started",
                {"output_root": "bad\npath"},
            ),
            "lua-message": _log_record(2, "lua.print", {"message": "bad\nline"}, level="debug"),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, unsafe in unsafe_cases.items():
                path = root / f"{name}.jsonl"
                records = [_log_record(1, "run.started", {}), unsafe, translation, finished]
                path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
                with self.subTest(name=name), self.assertRaises(ToolError):
                    summarize_att_run._summarize_one(path)

            safe_path = root / "safe-format.jsonl"
            safe_records = [
                _log_record(1, "run.started", {}),
                _log_record(
                    2,
                    "diagnostic.translation_task",
                    {
                        "relation": "primary",
                        "object": "对象\u200d名",
                        "reason": "原因",
                        "impact": "影响",
                        "help": "处理",
                    },
                    level="warn",
                ),
                translation,
                finished,
            ]
            safe_path.write_text(
                "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in safe_records),
                encoding="utf-8",
            )

            result = summarize_att_run._summarize_one(safe_path)

            self.assertEqual(result["diagnostics"][0]["object"], "对象\u200d名")

    def test_summarizer_ignores_unpaired_best_effort_phase_events(self) -> None:
        records = [
            _log_record(
                1,
                "run.started",
                {},
            ),
            _log_record(
                2,
                "phase.completed",
                {"phase": "planning", "amount": {"kind": "indeterminate"}},
            ),
            _log_record(
                3,
                "phase.started",
                {"phase": "scan_source", "amount": {"kind": "indeterminate"}},
            ),
            _log_record(4, "translation.finished", {"result": {"kind": "not_started"}}, level="warn"),
            _log_record(5, "run.finished", {"result": {"kind": "cancelled"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run-000001.jsonl"
            path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")

            result = summarize_att_run._summarize_one(path)

            self.assertEqual(result["phases"], [])

    def test_summarizer_rejects_unsafe_provider_and_retry_attempt_mismatch(self) -> None:
        for name, provider in (("newline", "bad\nprovider"), ("too-long", "x" * 129)):
            records = [
                _log_record(1, "run.started", {}),
                _log_record(2, "task.started", {"task": {"ordinal": 1, "total": 1}}),
                _log_record(
                    3,
                    "task.finished",
                    {
                        "task": {"ordinal": 1, "total": 1},
                        "attempts": 1,
                        "provider": provider,
                        "outcome": {"kind": "complete"},
                    },
                ),
                _log_record(4, "translation.finished", _generic_complete_result()),
                _log_record(5, "run.finished", {"result": {"kind": "succeeded"}}),
            ]
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / f"{name}.jsonl"
                path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
                with self.subTest(name=name), self.assertRaises(ToolError):
                    summarize_att_run._summarize_one(path)

        retry_mismatch = [
            _log_record(1, "run.started", {}),
            _log_record(2, "task.started", {"task": {"ordinal": 1, "total": 1}}),
            _log_record(
                3,
                "task.finished",
                {
                    "task": {"ordinal": 1, "total": 1},
                    "attempts": 3,
                    "provider": None,
                    "outcome": {"kind": "complete"},
                },
            ),
            _log_record(4, "retry.summary", {"attempted": 1, "recovered": 1, "exhausted": 0}),
            _log_record(5, "translation.finished", _generic_complete_result()),
            _log_record(6, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "retry.jsonl"
            path.write_text(
                "".join(json.dumps(record) + "\n" for record in retry_mismatch),
                encoding="utf-8",
            )
            with self.assertRaises(ToolError):
                summarize_att_run._summarize_one(path)

    def test_summarizer_requires_current_context_run_start_and_translate_terminal(self) -> None:
        context_mismatch = [
            _log_record(1, "run.started", {}),
            _log_record(2, "translation.finished", {"result": {"kind": "not_started"}}, level="warn"),
            _log_record(3, "run.finished", {"result": {"kind": "cancelled"}}),
        ]
        cast_context = context_mismatch[1]["context"]
        self.assertIsInstance(cast_context, dict)
        cast_context["project"] = "another-project"
        missing_start = [
            _log_record(1, "translation.finished", {"result": {"kind": "not_started"}}, level="warn"),
            _log_record(2, "run.finished", {"result": {"kind": "cancelled"}}),
        ]
        missing_translation = [
            _log_record(1, "run.started", {}),
            _log_record(2, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, records in (
                ("context", context_mismatch),
                ("start", missing_start),
                ("translation", missing_translation),
            ):
                path = root / f"{name}.jsonl"
                path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
                with self.subTest(name=name), self.assertRaises(ToolError):
                    summarize_att_run._summarize_one(path)

    def test_not_started_is_reported_as_incomplete(self) -> None:
        records = [
            _log_record(1, "run.started", {}),
            _log_record(2, "translation.finished", {"result": {"kind": "not_started"}}, level="warn"),
            _log_record(3, "run.finished", {"result": {"kind": "cancelled"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "run-000001.jsonl"
            output = root / "summary.json"
            log.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
            args = argparse.Namespace(log=[log], task_records=None, output=output, replace=False)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                summarize_att_run._summarize(args)
            self.assertIn("翻译未完整/失败/取消 1 次", stdout.getvalue())

    def test_summarizer_rejects_zero_attempts_and_invalid_retry_totals(self) -> None:
        bad_attempt = [
            _log_record(1, "run.started", {}),
            _log_record(2, "task.started", {"task": {"ordinal": 1, "total": 1}}),
            _log_record(
                3,
                "task.finished",
                {
                    "task": {"ordinal": 1, "total": 1},
                    "attempts": 0,
                    "provider": None,
                    "outcome": {"kind": "complete"},
                },
            ),
            _log_record(4, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        bad_retry = [
            _log_record(1, "run.started", {}),
            _log_record(2, "retry.summary", {"attempted": 1, "recovered": 0, "exhausted": 0}),
            _log_record(3, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        false_not_started = [
            _log_record(1, "run.started", {}),
            _log_record(2, "task.started", {"task": {"ordinal": 1, "total": 1}}),
            _log_record(
                3,
                "task.finished",
                {
                    "task": {"ordinal": 1, "total": 1},
                    "attempts": 1,
                    "provider": None,
                    "outcome": {"kind": "complete"},
                },
            ),
            _log_record(4, "translation.finished", {"result": {"kind": "not_started"}}, level="warn"),
            _log_record(5, "run.finished", {"result": {"kind": "succeeded"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, records in (
                ("attempt", bad_attempt),
                ("retry", bad_retry),
                ("not-started", false_not_started),
            ):
                path = root / f"{name}.jsonl"
                path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
                with self.subTest(name=name), self.assertRaises(ToolError):
                    summarize_att_run._summarize_one(path)

    def test_task_record_inventory_accepts_only_current_natural_names(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            valid = root / "run-000001"
            boundary = root / "run-999999"
            large = root / "run-1000000"
            invalid = root / "run-backup"
            overflow = root / "run-18446744073709551616"
            valid.mkdir()
            boundary.mkdir()
            large.mkdir()
            invalid.mkdir()
            overflow.mkdir()
            (valid / "task-000001.md").write_text("valid", encoding="utf-8")
            (valid / "task-000000.md").write_text("zero", encoding="utf-8")
            (valid / "task-0000001.md").write_text("overpadded", encoding="utf-8")
            (boundary / "task-999999.md").write_text("boundary", encoding="utf-8")
            (boundary / "task-1000000.md").write_text("boundary-large", encoding="utf-8")
            (large / "task-1000000.md").write_text("large", encoding="utf-8")
            (invalid / "task-not-natural.MD").write_text("invalid", encoding="utf-8")
            (overflow / "task-000001.md").write_text("overflow-run", encoding="utf-8")
            (valid / "task-18446744073709551616.md").write_text("overflow-task", encoding="utf-8")

            result = summarize_att_run._task_records(root)

            self.assertEqual(result["count"], 4)
            self.assertEqual(
                list(result["runs"]),
                ["run-000001", "run-999999", "run-1000000"],
            )
            self.assertEqual(
                result["runs"],
                {
                    "run-000001": ["task-000001.md"],
                    "run-999999": ["task-999999.md", "task-1000000.md"],
                    "run-1000000": ["task-1000000.md"],
                },
            )

    def test_summarizer_rejects_run_id_above_u64(self) -> None:
        record = _log_record(1, "run.started", {})
        record["run_id"] = "run-18446744073709551616"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "overflow.jsonl"
            path.write_text(json.dumps(record) + "\n", encoding="utf-8")

            with self.assertRaises(ToolError):
                summarize_att_run._summarize_one(path)


if __name__ == "__main__":
    unittest.main()
