from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

import translation_qa
from att_skill_tools import ToolError
from att_toolbox.adapter_verification import AdapterVerification, read_adapter_report, verify_report
from att_toolbox.translation_export import read_translation_document
from att_toolbox.translation_review import adjacent_review


class AdapterVerificationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        self.output = self.root / "output"
        self.input = self.root / "input"
        self.write_back = self.root / "write-back"
        for path in (self.source, self.output, self.input, self.write_back):
            path.mkdir()
        self.identifier = "scene.jsonl:line1:unit1:text"
        self.source_text = "【ステラ】テスト"
        self.target_text = "【ステラ】中文"
        (self.source / "scene.bin").write_bytes('【ステラ】"テスト"'.encode("cp932"))
        (self.output / "scene.bin").write_bytes('【ステラ】"中文"'.encode("cp932"))
        (self.output / "font.bin").write_bytes(b"font baseline")
        self.verifier = self.root / "native-reader.py"
        self.verifier.write_text("# 姓名控制段在引用状态之外解析\n", encoding="utf-8")
        self.translations = self.root / "translations.jsonl"
        self.translations.write_text(
            json.dumps({"source_language": "ja", "target_language": "zh-Hans"})
            + "\n"
            + json.dumps(
                {
                    "manual_id": self.identifier,
                    "group": self.identifier,
                    "source": [self.source_text],
                    "translation": [self.target_text],
                    "type": "free",
                    "state": "current",
                    "origin": "manual",
                    "reviews": [],
                    "natural_source": [None, "テスト"],
                    "natural_translation": [None, "中文"],
                },
                ensure_ascii=False,
            )
            + "\n",
            encoding="utf-8",
        )
        for root, text in ((self.input, self.source_text), (self.write_back, self.target_text)):
            (root / "scene.jsonl").write_text(
                json.dumps(
                    {
                        "id": "scene",
                        "kind": "dialogue",
                        "units": [{"id": "body", "text": text}],
                    },
                    ensure_ascii=False,
                )
                + "\n",
                encoding="utf-8",
                newline="\n",
            )

    def capture(self) -> AdapterVerification:
        evidence = AdapterVerification(
            translations=self.translations,
            generic_input=self.input,
            write_back=self.write_back,
            source_root=self.source,
            output_root=self.output,
            source_files=["scene.bin"],
            output_files=["scene.bin", "font.bin"],
            verifier_files=[self.verifier],
        )
        source_bytes = (self.source / "scene.bin").read_bytes()
        output_bytes = (self.output / "scene.bin").read_bytes()
        evidence.observe_unit(
            self.identifier,
            source_file="scene.bin",
            source_location="对白 1",
            source_text=source_bytes.decode("cp932").replace('"', ""),
            output_file="scene.bin",
            output_location="对白 1",
            output_text=output_bytes.decode("cp932").replace('"', ""),
            display_text="斯黛拉：中文",
        )
        evidence.check(
            "姓名进入原生控制分支",
            kind="consumer",
            method="native_static",
            manual_ids=[self.identifier],
            status="passed" if output_bytes.startswith("【ステラ】".encode("cp932")) else "failed",
            details="核对姓名前缀是否处在字符串引号外；普通文本回读会移除引号，不能独立证明这个条件。",
            output_files=["scene.bin"],
        )
        evidence.check(
            "中文替换往返",
            kind="changed_text_roundtrip",
            method="native_static",
            manual_ids=[self.identifier],
            status="passed",
            details="读取实际替换后的对白，保持姓名语法位置。",
            source_files=["scene.bin"],
            output_files=["scene.bin"],
        )
        return evidence

    def inspect(self, evidence: AdapterVerification):
        return verify_report(
            evidence.report,
            self.translations,
            self.input,
            self.write_back,
            read_translation_document(self.translations).rows,
        )

    def test_text_roundtrip_cannot_hide_broken_name_control(self) -> None:
        (self.output / "scene.bin").write_bytes('"【ステラ】中文"'.encode("cp932"))
        evidence = self.capture()
        findings, unverified, summary = self.inspect(evidence)
        self.assertEqual(summary["output_units_verified"], 1)
        self.assertEqual(summary["consumer_units_verified"], 0)
        self.assertIn("generic_actual_consumer_unverified", unverified)
        self.assertEqual(findings[0]["kind"], "external_consumer_failure")
        self.assertEqual(findings[0]["repair_stage"], "adapter")

    def test_static_checks_keep_runtime_and_unmeasured_layout_explicit(self) -> None:
        evidence = self.capture()
        findings, unverified, summary = self.inspect(evidence)
        self.assertFalse(findings)
        self.assertEqual(summary["consumer_units_verified"], 1)
        self.assertEqual(set(unverified), {"runtime_scenarios_unverified", "layout_unverified"})
        evidence.measure(
            self.identifier,
            text="旧译文",
            width=20,
            height=20,
            available_width=40,
            available_height=40,
            method="native_static",
            details="原生绘字计算",
        )
        with self.assertRaises(ToolError):
            self.inspect(evidence)

    def test_changed_artifacts_and_verifier_invalidate_report(self) -> None:
        for path in (self.output / "font.bin", self.output / "scene.bin", self.verifier, self.translations):
            with self.subTest(path=path.name):
                original = path.read_bytes()
                evidence = self.capture()
                path.write_bytes(original + b" ")
                with self.assertRaises(ToolError):
                    self.inspect(evidence)
                path.write_bytes(original)

    def test_missing_and_duplicate_observations_do_not_claim_coverage(self) -> None:
        evidence = self.capture()
        evidence.units.clear()
        evidence.checks.clear()
        _, unverified, summary = self.inspect(evidence)
        self.assertEqual(summary["source_units_verified"], 0)
        self.assertIn("generic_reverse_conversion_unverified", unverified)
        evidence = self.capture()
        evidence.units.append(evidence.units[0])
        with self.assertRaises(ToolError):
            self.inspect(evidence)

    def test_scan_consumes_evidence_and_rechecks_it_before_manual_selection(self) -> None:
        evidence = self.capture()
        report = self.root / "adapter.json"
        evidence.write(report)
        qa = self.root / "qa"
        args = translation_qa._parser().parse_args(
            [
                "scan",
                "--translations",
                str(self.translations),
                "--generic-input",
                str(self.input),
                "--write-back",
                str(self.write_back),
                "--adapter-report",
                str(report),
                "--output",
                str(qa),
            ]
        )
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(translation_qa._scan(args), 0)
        summary = json.loads((qa / "qa-summary.json").read_text(encoding="utf-8"))
        self.assertEqual(summary["adapter_verification"]["output_units_verified"], 1)
        self.assertNotIn("layout_risk", summary["counts"])
        self.assertNotIn("source_residual", summary["counts"])
        self.assertEqual(summary["target_language"], "zh-Hans")
        self.assertEqual(read_adapter_report(report)["output_root"], str(self.output))
        (self.output / "font.bin").write_bytes(b"new font")
        selection = translation_qa._parser().parse_args(
            ["manual", "--scan", str(qa), "--output", str(self.root / "ids.jsonl")]
        )
        with self.assertRaises(ToolError):
            translation_qa._manual(selection)


class AdjacentReviewTests(unittest.TestCase):
    def test_added_utterance_is_reviewed_but_plain_reflow_is_not(self) -> None:
        row = {
            "manual_id": "dialogue",
            "group": "dialogue",
            "source": ["「ここで待っていて。」"],
            "translation": ["「在这里等我。」", "「我会马上回来。」"],
            "state": "current",
        }
        findings = adjacent_review([row], {}, [], source_language="ja", target_language="zh-Hans")
        self.assertEqual([item["kind"] for item in findings], ["additional_quoted_passage"])
        row["translation"] = ["「在这里", "等我。」"]
        self.assertFalse(adjacent_review([row], {}, [], source_language="ja", target_language="zh-Hans"))

    def test_shifted_story_and_merged_neighbor_are_reviewed_without_crossing_scope(self) -> None:
        sources = [
            "王城の地下には秘密の石碑がある。",
            "海辺の村には巨大な灯台がある。",
            "雪山の洞窟には銀色の宝石がある。",
        ]
        targets = [
            "海边的村落有一座巨大的灯塔。",
            "雪山洞窟里面藏着一颗银色宝石。",
            "雪山洞窟里面藏着一颗银色宝石。海边的村落有一座巨大的灯塔。",
        ]
        rows = [
            {
                "manual_id": str(i),
                "group": str(i),
                "source": [source],
                "translation": [target],
                "state": "current",
            }
            for i, (source, target) in enumerate(zip(sources, targets, strict=True))
        ]
        findings = adjacent_review(
            rows,
            {str(i): "scene" for i in range(3)},
            [("石碑", "石碑")],
            source_language="ja",
            target_language="zh-Hans",
        )
        self.assertIn("adjacent_alignment_review", {item["kind"] for item in findings})
        self.assertIn("adjacent_content_overlap", {item["kind"] for item in findings})
        self.assertEqual(findings[0]["review_ids"], ["0", "1", "2"])
        isolated = adjacent_review(rows, {}, [], source_language="ja", target_language="zh-Hans")
        self.assertFalse(isolated)
        repeated = [
            {**rows[0], "source": [sources[0]], "translation": ["第一句话的正常中文译文。", "第二句话。"]},
            {**rows[1], "source": [sources[0]], "translation": ["第一句话的正常中文译文。", "第二句话。"]},
        ]
        self.assertFalse(
            adjacent_review(
                repeated, {"0": "scene", "1": "scene"}, [], source_language="ja", target_language="zh-Hans"
            )
        )


if __name__ == "__main__":
    unittest.main()
