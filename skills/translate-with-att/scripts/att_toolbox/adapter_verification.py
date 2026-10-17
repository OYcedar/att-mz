"""Generic 外部适配器的旁路验收证据；不参与 ATT 状态、恢复或发布准入。"""

from __future__ import annotations

import hashlib
import json
import math
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

from att_skill_tools import (
    JsonValue,
    atomic_write_text,
    fail,
    protect_outputs,
    read_json_object,
    require_directory,
    require_file,
    require_file_within,
    safe_walk_files,
    validate_object_keys,
)

from .translation_export import read_translation_export


def file_binding(path: Path) -> dict[str, JsonValue]:
    path = require_file(path, "验收文件")
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"path": str(path.resolve()), "sha256": digest}


def tree_binding(path: Path) -> dict[str, JsonValue]:
    root = require_directory(path, "Generic JSONL 根")
    return {
        "path": str(root.resolve()),
        "files": {
            file.relative_to(root).as_posix(): file_binding(file)["sha256"]
            for file in sorted(safe_walk_files(root))
            if file.suffix == ".jsonl"
        },
    }


def _files(root: Path, names: Sequence[str]) -> dict[str, JsonValue]:
    return {
        name: file_binding(require_file_within(root / name, root, "适配器证据文件"))["sha256"]
        for name in names
    }


class AdapterVerification:
    """先冻结本次读写文件，再由独立读取器提交观察，最后重新核对基线。"""

    def __init__(
        self,
        *,
        translations: Path,
        generic_input: Path,
        write_back: Path,
        source_root: Path,
        output_root: Path,
        source_files: Sequence[str],
        output_files: Sequence[str],
        verifier_files: Sequence[Path],
    ) -> None:
        self.translations = translations.resolve()
        self.generic_input = generic_input.resolve()
        self.write_back = write_back.resolve()
        self.source_root = require_directory(source_root, "游戏来源根").resolve()
        self.output_root = require_directory(output_root, "最终游戏根").resolve()
        if (
            self.source_root == self.output_root
            or self.source_root in self.output_root.parents
            or self.output_root in self.source_root.parents
        ):
            fail(str(output_root), "来源根和输出根重叠", "使用独立游戏副本")
        self.units: list[JsonValue] = []
        self.checks: list[JsonValue] = []
        self.measurements: list[JsonValue] = []
        self.report: dict[str, JsonValue] = {
            "translations": file_binding(self.translations),
            "generic_input": tree_binding(self.generic_input),
            "write_back": tree_binding(self.write_back),
            "source_root": str(self.source_root),
            "output_root": str(self.output_root),
            "source_files": _files(self.source_root, source_files),
            "output_files": _files(self.output_root, output_files),
            "verifier_files": {
                str(path.resolve()): file_binding(path)["sha256"]
                for path in (Path(__file__), *verifier_files)
            },
            "units": self.units,
            "checks": self.checks,
            "measurements": self.measurements,
        }

    def observe_unit(
        self,
        manual_id: str,
        *,
        source_file: str,
        source_location: str,
        source_text: str,
        output_file: str,
        output_location: str,
        output_text: str,
        display_text: str | None = None,
    ) -> None:
        observation: dict[str, JsonValue] = {
            "manual_id": manual_id,
            "source_file": source_file,
            "source_location": source_location,
            "source_text": source_text,
            "output_file": output_file,
            "output_location": output_location,
            "output_text": output_text,
        }
        if display_text is not None:
            observation["display_text"] = display_text
        self.units.append(observation)

    def check(
        self,
        name: str,
        *,
        kind: str,
        method: str,
        manual_ids: Sequence[str],
        status: str,
        details: str,
        source_files: Sequence[str] = (),
        output_files: Sequence[str] = (),
    ) -> None:
        self.checks.append(
            {
                "name": name,
                "kind": kind,
                "method": method,
                "manual_ids": list(manual_ids),
                "status": status,
                "details": details,
                "source_files": list(source_files),
                "output_files": list(output_files),
            }
        )

    def measure(
        self,
        manual_id: str,
        *,
        text: str,
        width: float,
        height: float,
        available_width: float,
        available_height: float,
        method: str,
        details: str,
    ) -> None:
        self.measurements.append(
            {
                "manual_id": manual_id,
                "text": text,
                "width": width,
                "height": height,
                "available_width": available_width,
                "available_height": available_height,
                "method": method,
                "details": details,
            }
        )

    def write(self, path: Path, *, replace: bool = False) -> None:
        protect_outputs(
            [path],
            inputs=[self.translations, self.generic_input, self.write_back],
            forbidden_roots=[self.source_root, self.output_root],
            replace=replace,
        )
        verify_report(
            self.report,
            self.translations,
            self.generic_input,
            self.write_back,
            read_translation_export(self.translations),
        )
        atomic_write_text(path, json.dumps(self.report, ensure_ascii=False, indent=2) + "\n", replace=replace)


def _string(value: JsonValue, field: str, *, empty: bool = False) -> str:
    if not isinstance(value, str) or (not empty and not value.strip()):
        fail("外部验收报告", f"{field} 不是有效字符串", "重新运行适配器验收")
    return value


def _objects(value: JsonValue, field: str) -> list[dict[str, JsonValue]]:
    if not isinstance(value, list) or any(not isinstance(item, dict) for item in value):
        fail("外部验收报告", f"{field} 不是 object 数组", "重新运行适配器验收")
    return cast(list[dict[str, JsonValue]], value)


def _strings(value: JsonValue, field: str) -> list[str]:
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        fail("外部验收报告", f"{field} 不是字符串数组", "重新运行适配器验收")
    items = cast(list[str], value)
    if len(items) != len(set(items)):
        fail("外部验收报告", f"{field} 包含重复项", "重新运行适配器验收")
    return items


def verify_bindings(
    report: Mapping[str, JsonValue], translations: Path, generic_input: Path, write_back: Path
) -> None:
    roots = [
        require_directory(Path(_string(report.get(f"{side}_root"), f"{side}_root")), "验收资源根").resolve()
        for side in ("source", "output")
    ]
    if roots[0] == roots[1] or roots[0] in roots[1].parents or roots[1] in roots[0].parents:
        fail("外部验收报告", "来源与输出范围重叠", "使用独立游戏副本")
    actual = {
        "translations": file_binding(translations),
        "generic_input": tree_binding(generic_input),
        "write_back": tree_binding(write_back),
    }
    for name, expected in actual.items():
        if report.get(name) != expected:
            fail("外部验收报告", f"{name} 与当前产物不一致，证据已失效", "重新运行适配器验收")
    for side in ("source", "output"):
        root = require_directory(Path(_string(report.get(f"{side}_root"), f"{side}_root")), "验收资源根")
        files = report.get(f"{side}_files")
        if not isinstance(files, dict) or not files:
            fail(
                "外部验收报告", f"{side}_files 缺少实际读取文件", "记录全部被检查的资源、解析代码、配置与字体"
            )
        if _files(root, list(files)) != files:
            fail(str(root), "验收后资源发生变化，证据已失效", "重新运行适配器验收")
    verifiers = report.get("verifier_files")
    if not isinstance(verifiers, dict) or not verifiers:
        fail("外部验收报告", "缺少读取器与验证程序基线", "记录实际使用的验证程序")
    for name, expected in verifiers.items():
        if file_binding(Path(name))["sha256"] != expected:
            fail(name, "验收程序发生变化，证据已失效", "重新运行适配器验收")


def _write_back_texts(root: Path) -> dict[str, str]:
    from att_skill_tools import parse_json_text, physical_jsonl_lines, read_physical_text

    result: dict[str, str] = {}
    for path in sorted(safe_walk_files(root)):
        if path.suffix != ".jsonl":
            continue
        for line, text in physical_jsonl_lines(read_physical_text(path), str(path)):
            group = parse_json_text(text, str(path))
            if not isinstance(group, dict) or not isinstance(group.get("units"), list):
                fail(str(path), "不是 Generic Group", "重新生成 WriteBack")
            for unit_number, unit in enumerate(cast(list[JsonValue], group["units"]), 1):
                if not isinstance(unit, dict) or not isinstance(unit.get("text"), str):
                    fail(str(path), "不是 Generic Unit", "重新生成 WriteBack")
                result[f"{path.relative_to(root).as_posix()}:line{line}:unit{unit_number}:text"] = cast(
                    str, unit["text"]
                )
    return result


def verify_report(
    report: Mapping[str, JsonValue],
    translations: Path,
    generic_input: Path,
    write_back: Path,
    rows: Sequence[Mapping[str, JsonValue]],
) -> tuple[list[dict[str, JsonValue]], list[str], dict[str, JsonValue]]:
    fields = {
        "translations",
        "generic_input",
        "write_back",
        "source_root",
        "output_root",
        "source_files",
        "output_files",
        "verifier_files",
        "units",
        "checks",
        "measurements",
    }
    validate_object_keys(report, "外部验收报告", fields)
    if set(report) != fields:
        fail("外部验收报告", "字段不完整", "重新运行适配器验收")
    verify_bindings(report, translations, generic_input, write_back)
    by_id = {cast(str, row["manual_id"]): row for row in rows}
    expected = _write_back_texts(write_back)
    if set(expected) != set(by_id):
        fail("外部验收报告", "WriteBack 与完整译文导出的自然 ID 不一致", "重新生成 WriteBack 和验收证据")
    findings: list[dict[str, JsonValue]] = []
    observed: dict[str, dict[str, JsonValue]] = {}
    mapped: set[str] = set()
    reversed_ids: set[str] = set()

    def finding(
        kind: str, identifier: str | None, detail: Mapping[str, JsonValue], *, confirmed: bool = True
    ) -> None:
        item: dict[str, JsonValue] = {
            "kind": kind,
            "analysis_status": "confirmed_fact" if confirmed else "heuristic_review",
            **detail,
        }
        if kind.startswith("external_"):
            item["repair_stage"] = "adapter"
        if identifier is not None:
            item["manual_id"] = identifier
        findings.append(item)

    for unit in _objects(report["units"], "units"):
        validate_object_keys(
            unit,
            "验收 Unit",
            {
                "manual_id",
                "source_file",
                "source_location",
                "source_text",
                "output_file",
                "output_location",
                "output_text",
                "display_text",
            },
        )
        identifier = _string(unit.get("manual_id"), "manual_id")
        if identifier not in by_id or identifier in observed:
            fail("外部验收报告", f"未知或重复的 Unit：{identifier}", "使用完整当前导出的自然 ID")
        for side in ("source", "output"):
            file = _string(unit.get(f"{side}_file"), f"{side}_file")
            if file not in cast(dict[str, JsonValue], report[f"{side}_files"]):
                fail(identifier, "观察位置没有绑定实际文件", "把读取资源加入文件基线")
            _string(unit.get(f"{side}_location"), f"{side}_location")
            _string(unit.get(f"{side}_text"), f"{side}_text", empty=True)
        if "display_text" in unit:
            _string(unit["display_text"], "display_text", empty=True)
        observed[identifier] = unit
        if unit["source_text"] == "\n".join(cast(list[str], by_id[identifier]["source"])):
            mapped.add(identifier)
        else:
            finding(
                "external_source_mismatch",
                identifier,
                {"location": unit["source_location"], "file": unit["source_file"]},
            )
        if unit["output_text"] == expected[identifier]:
            reversed_ids.add(identifier)
        else:
            finding(
                "external_output_mismatch",
                identifier,
                {"location": unit["output_location"], "file": unit["output_file"]},
            )

    consumers: set[str] = set()
    runtime: set[str] = set()
    failed_consumers: set[str] = set()
    changed_probe = False
    names: set[str] = set()
    for check in _objects(report["checks"], "checks"):
        validate_object_keys(
            check,
            "消费者检查",
            {"name", "kind", "method", "manual_ids", "status", "details", "source_files", "output_files"},
        )
        name = _string(check.get("name"), "check.name")
        kind, method, status = check.get("kind"), check.get("method"), check.get("status")
        if (
            name in names
            or kind not in ("consumer", "changed_text_roundtrip")
            or method not in ("native_static", "runtime")
            or status not in ("passed", "failed", "unverified")
        ):
            fail(name, "检查名称重复或类型、方法、结果无效", "重新运行适配器验收")
        names.add(name)
        ids = _strings(check.get("manual_ids"), "check.manual_ids")
        if not ids or any(identifier not in observed for identifier in ids):
            fail(name, "检查没有对应已观察 Unit", "先记录实际读取的源文和最终译文")
        _string(check.get("details"), "check.details")
        evidence_files = 0
        for side in ("source", "output"):
            files = _strings(check.get(f"{side}_files"), f"check.{side}_files")
            if any(file not in cast(dict[str, JsonValue], report[f"{side}_files"]) for file in files):
                fail(name, "检查证据未绑定文件", "记录实际使用的引擎、封包、配置、字体或独立读取器")
            evidence_files += len(files)
        if not evidence_files:
            fail(name, "检查没有文件证据", "记录实际检查所用文件")
        if kind == "changed_text_roundtrip" and status == "passed":
            if not any(
                observed[identifier]["source_text"] != observed[identifier]["output_text"]
                for identifier in ids
            ):
                fail(name, "变更文本探针没有实际改写正文", "用真实替换文本验证回填路径")
            changed_probe = True
        if kind == "consumer":
            if status == "passed":
                consumers.update(ids)
                if method == "runtime":
                    runtime.update(ids)
            else:
                failed_consumers.update(ids)
        if status != "passed":
            finding(
                "external_consumer_failure" if status == "failed" else "external_check_unverified",
                None,
                {"check": name, "details": check["details"], "review_ids": ids},
                confirmed=status == "failed",
            )
    consumers.difference_update(failed_consumers)
    runtime.difference_update(failed_consumers)
    measured: set[str] = set()
    for measurement in _objects(report["measurements"], "measurements"):
        validate_object_keys(
            measurement,
            "布局测量",
            {
                "manual_id",
                "text",
                "width",
                "height",
                "available_width",
                "available_height",
                "method",
                "details",
            },
        )
        identifier = _string(measurement.get("manual_id"), "measurement.manual_id")
        if identifier not in observed or measurement.get("text") != observed[identifier].get(
            "display_text", observed[identifier]["output_text"]
        ):
            fail(identifier, "布局测量文本不对应最终消费者文字", "重新测量最终绘制文本")
        if measurement.get("method") not in ("native_static", "runtime"):
            fail(identifier, "布局测量方法无效", "使用实际引擎布局或运行时测量")
        _string(measurement.get("details"), "measurement.details")
        dimensions: dict[str, float] = {}
        for field in ("width", "height", "available_width", "available_height"):
            value = measurement.get(field)
            if (
                not isinstance(value, (float, int))
                or isinstance(value, bool)
                or not math.isfinite(value)
                or value < 0
                or (field.startswith("available_") and value == 0)
            ):
                fail(identifier, "布局像素尺寸无效", "记录真实可用区域与完整文字尺寸")
            dimensions[field] = float(value)
        measured.add(identifier)
        if (
            dimensions["width"] > dimensions["available_width"]
            or dimensions["height"] > dimensions["available_height"]
        ):
            finding("layout_overflow", identifier, measurement, confirmed=False)
    all_ids = set(by_id)
    unverified = [
        name
        for name, coverage in (
            ("generic_external_source_mapping_unverified", mapped),
            ("generic_reverse_conversion_unverified", reversed_ids),
            ("generic_actual_consumer_unverified", consumers),
            ("runtime_scenarios_unverified", runtime),
            ("layout_unverified", measured),
        )
        if coverage != all_ids
    ]
    if all_ids and not changed_probe:
        unverified.append("generic_changed_text_probe_unverified")
    summary: dict[str, JsonValue] = {
        "source_units_verified": len(mapped),
        "output_units_verified": len(reversed_ids),
        "consumer_units_verified": len(consumers),
        "runtime_units_observed": len(runtime),
        "layout_units_measured": len(measured),
        "changed_text_probe": changed_probe,
        "source_root": report["source_root"],
        "output_root": report["output_root"],
    }
    return findings, unverified, summary


def read_adapter_report(path: Path) -> dict[str, JsonValue]:
    return read_json_object(path, "Generic 外部验收报告")
