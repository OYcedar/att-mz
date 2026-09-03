from __future__ import annotations

import argparse
import builtins
import contextlib
import io
import json
import sys
import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import TextIO
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

import rpg_maker_survey
import summarize_att_run
import translation_preflight
import translation_qa
from att_skill_tools import run_cli


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")


def _write_mz_game(root: Path, *, custom_text: str | None = None) -> None:
    (root / "data").mkdir(parents=True)
    (root / "js").mkdir()
    (root / "js" / "rmmz_core.js").write_text("// core\n", encoding="utf-8")
    (root / "js" / "plugins.js").write_text("var $plugins = [];\n", encoding="utf-8")
    _write_json(
        root / "data" / "System.json",
        {
            "gameTitle": "",
            "currencyUnit": "",
            "terms": {"basic": [], "commands": [], "params": [], "messages": {}},
            "elements": [],
            "skillTypes": [],
            "weaponTypes": [],
            "armorTypes": [],
            "equipTypes": [],
        },
    )
    if custom_text is not None:
        _write_json(root / "data" / "Custom.json", {"text": custom_text})


def _stdout_failure(error: BaseException) -> Callable[..., None]:
    original_print = builtins.print

    def injected(
        *args: object,
        sep: str | None = " ",
        end: str | None = "\n",
        file: TextIO | None = None,
        flush: bool = False,
    ) -> None:
        if file is None:
            raise error
        original_print(*args, sep=sep, end=end, file=file, flush=flush)

    return injected


def _run_with_stdout_failure(command: Callable[[], int], error: BaseException) -> int:
    stderr = io.StringIO()
    with (
        contextlib.redirect_stderr(stderr),
        patch(
            "builtins.print",
            side_effect=_stdout_failure(error),
        ),
    ):
        try:
            run_cli(command)
        except SystemExit as exit_error:
            if isinstance(exit_error.code, int):
                return exit_error.code
            raise AssertionError(f"run_cli 返回非整数退出码：{exit_error.code}") from None
    raise AssertionError("run_cli 没有退出")


def _run_success(command: Callable[[], int]) -> None:
    with contextlib.redirect_stdout(io.StringIO()):
        try:
            run_cli(command)
        except SystemExit as exit_error:
            if exit_error.code != 0:
                raise


def _survey_args(game: Path, output: Path) -> argparse.Namespace:
    return argparse.Namespace(game=game, output=output, replace=False)


def _finalize_args(survey: Path, output: Path) -> argparse.Namespace:
    return argparse.Namespace(survey=survey, decisions=None, output=output, replace=False)


class PublishedCompletionEntrypointTests(unittest.TestCase):
    def test_survey_and_preflight_outputs_survive_completion_output_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            game = root / "game"
            _write_mz_game(game)
            survey = root / "survey"

            status = _run_with_stdout_failure(
                lambda: rpg_maker_survey._scan(_survey_args(game, survey)),  # pyright: ignore[reportPrivateUsage]
                BrokenPipeError("stdout closed"),
            )
            self.assertEqual(status, 1)
            self.assertTrue((survey / "survey.json").is_file())

            plan = root / "plan"
            status = _run_with_stdout_failure(
                lambda: rpg_maker_survey._finalize(  # pyright: ignore[reportPrivateUsage]
                    _finalize_args(survey, plan)
                ),
                KeyboardInterrupt(),
            )
            self.assertEqual(status, 130)
            self.assertTrue((plan / "coverage.json").is_file())

            ownership = root / "ownership.jsonl"
            ownership.write_text("", encoding="utf-8")
            audit = root / "audit.json"
            audit_args = argparse.Namespace(
                survey=survey,
                plan=plan,
                ownership=ownership,
                output=audit,
                replace=False,
            )
            status = _run_with_stdout_failure(
                lambda: rpg_maker_survey._audit(audit_args),  # pyright: ignore[reportPrivateUsage]
                BrokenPipeError("stdout closed"),
            )
            self.assertEqual(status, 1)
            self.assertTrue(audit.is_file())

            manual = root / "manual.toml"
            manual.write_text("translation = []\n", encoding="utf-8")
            preflight = root / "preflight"
            preflight_args = argparse.Namespace(
                manual=manual,
                survey=survey,
                coverage=plan / "coverage.json",
                decisions=None,
                output=preflight,
                replace=False,
            )
            status = _run_with_stdout_failure(
                lambda: translation_preflight._run(preflight_args),  # pyright: ignore[reportPrivateUsage]
                KeyboardInterrupt(),
            )
            self.assertEqual(status, 130)
            self.assertTrue((preflight / "preflight.json").is_file())

            review_game = root / "review-game"
            _write_mz_game(review_game, custom_text="Visible custom text")
            review_survey = root / "review-survey"
            _run_success(
                lambda: rpg_maker_survey._scan(  # pyright: ignore[reportPrivateUsage]
                    _survey_args(review_game, review_survey)
                )
            )
            group = json.loads(
                (review_survey / "review-groups.jsonl").read_text(encoding="utf-8").splitlines()[0]
            )
            candidates = root / "candidate-decisions.jsonl"
            members_args = argparse.Namespace(
                survey=review_survey,
                group_id=group["group_id"],
                output=candidates,
                replace=False,
            )
            status = _run_with_stdout_failure(
                lambda: rpg_maker_survey._members(members_args),  # pyright: ignore[reportPrivateUsage]
                KeyboardInterrupt(),
            )
            self.assertEqual(status, 130)
            self.assertTrue(candidates.is_file())

    def test_qa_and_summary_outputs_survive_completion_output_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generic_input = root / "generic-input"
            _write_json(
                generic_input / "sample.jsonl",
                {"id": "group", "kind": "entry", "units": [{"id": "unit", "text": "Source"}]},
            )
            translations = root / "translations.jsonl"
            _write_json(
                translations,
                {
                    "manual_id": "sample.jsonl:line1:unit1:text",
                    "source": ["Source"],
                    "translation": ["译文"],
                    "state": "current",
                    "origin": "manual",
                    "type": "free",
                    "owner": None,
                    "rule_number": None,
                },
            )
            qa = root / "qa"
            qa_args = argparse.Namespace(
                translations=translations,
                survey=None,
                generic_input=generic_input,
                coverage=None,
                generic_manifest=None,
                terminology=None,
                write_back=None,
                runtime_report=None,
                output=qa,
                replace=False,
            )
            status = _run_with_stdout_failure(
                lambda: translation_qa._scan(qa_args),  # pyright: ignore[reportPrivateUsage]
                BrokenPipeError("stdout closed"),
            )
            self.assertEqual(status, 1)
            self.assertTrue((qa / "qa-summary.json").is_file())

            candidates = root / "qa-candidates.jsonl"
            manual_args = argparse.Namespace(scan=qa, review_group=[], output=candidates, replace=False)
            status = _run_with_stdout_failure(
                lambda: translation_qa._manual(manual_args),  # pyright: ignore[reportPrivateUsage]
                KeyboardInterrupt(),
            )
            self.assertEqual(status, 130)
            self.assertTrue(candidates.is_file())

            log = root / "run-000001.jsonl"
            records = [
                {
                    "timestamp": "2026-01-01T00:00:01Z",
                    "sequence": 1,
                    "run_id": "run-000001",
                    "level": "info",
                    "event": "run.started",
                    "context": {
                        "locale": "zh-Hans",
                        "engine": "generic",
                        "project": "test",
                        "command": "translate",
                    },
                    "payload": {},
                    "message": "run.started",
                },
                {
                    "timestamp": "2026-01-01T00:00:02Z",
                    "sequence": 2,
                    "run_id": "run-000001",
                    "level": "warn",
                    "event": "translation.finished",
                    "context": {
                        "locale": "zh-Hans",
                        "engine": "generic",
                        "project": "test",
                        "command": "translate",
                    },
                    "payload": {"result": {"kind": "not_started"}},
                    "message": "translation.finished",
                },
                {
                    "timestamp": "2026-01-01T00:00:03Z",
                    "sequence": 3,
                    "run_id": "run-000001",
                    "level": "info",
                    "event": "run.finished",
                    "context": {
                        "locale": "zh-Hans",
                        "engine": "generic",
                        "project": "test",
                        "command": "translate",
                    },
                    "payload": {"result": {"kind": "cancelled"}},
                    "message": "run.finished",
                },
            ]
            log.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
            summary = root / "summary.json"
            summary_args = argparse.Namespace(log=[log], task_records=None, output=summary, replace=False)
            status = _run_with_stdout_failure(
                lambda: summarize_att_run._summarize(summary_args),  # pyright: ignore[reportPrivateUsage]
                BrokenPipeError("stdout closed"),
            )
            self.assertEqual(status, 1)
            self.assertTrue(summary.is_file())


if __name__ == "__main__":
    unittest.main()
