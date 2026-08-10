"""RPG Maker 来源扫描、精确位置投影和关系组实现。"""

from __future__ import annotations

import json
import time
from collections.abc import Iterable, Mapping
from pathlib import Path

from att_skill_tools import JsonValue, ToolError, fail, parse_json_text, safe_walk_files

from .resources import classify_resource_reference
from .rpg import (
    STANDARD_DATA_FILES,
    GameInfo,
    actual_path,
    discover_game,
    is_builtin_data_path,
    iter_string_leaves,
    normalized_path,
    parse_plugins,
    require_game_root,
)
from .rpg_control_codes import is_structural_blank, split_rpg_text_lines
from .survey_code import scan_code_sources
from .survey_identity import rule_manual_id
from .survey_io import decode_text, file_bytes
from .survey_model import FileSnapshot, LocationFact, SurveyBundle
from .survey_relations import review_groups as build_review_groups
from .survey_rpg_data import (
    builtin_database_locations,
    builtin_event_locations,
    builtin_system_locations,
    canonical_map_number,
    event_lists,
    is_builtin_command_leaf,
)
from .survey_suggestions import lexical_suggestion, rule_proposal

GENERIC_EVIDENCE_FIELDS = (
    "exact_location",
    "active_runtime_consumer",
    "player_visible_non_image_text",
    "builtin_not_owner",
    "rules_cannot_map_reversibly",
    "extract_group_unit_write_back_mapping",
    "unique_owner",
)

_EXTERNAL_TEXT_SUFFIXES = frozenset(
    {
        ".cfg",
        ".conf",
        ".csv",
        ".htm",
        ".html",
        ".ini",
        ".json",
        ".markdown",
        ".md",
        ".po",
        ".properties",
        ".srt",
        ".toml",
        ".tsv",
        ".txt",
        ".vtt",
        ".xml",
        ".yaml",
        ".yml",
    }
)


def scan_game(game_path: Path) -> SurveyBundle:
    started = time.perf_counter()
    game: GameInfo = discover_game(game_path)
    game_root = require_game_root(game)
    files = list(safe_walk_files(game_root))
    files.sort(key=lambda path: path.relative_to(game_root).as_posix().encode("utf-8"))
    data_root = (game.content_root / "data").resolve(strict=True)
    data_paths = [path for path in files if path.parent == data_root and path.suffix.lower() == ".json"]
    snapshots: dict[str, FileSnapshot] = {}
    raw_files: dict[str, bytes] = {}
    files_read = 0

    def read_once(path: Path) -> tuple[bytes, FileSnapshot]:
        nonlocal files_read
        relative = path.relative_to(game_root).as_posix()
        if relative in snapshots:
            return raw_files[relative], snapshots[relative]
        raw, snapshot = file_bytes(path, game_root)
        raw_files[relative] = raw
        snapshots[relative] = snapshot
        files_read += 1
        return raw, snapshot

    plugins_js = game.content_root / "js" / "plugins.js"
    if not plugins_js.is_file():
        fail(str(plugins_js), "缺少 RPG Maker plugins.js", "恢复实际内容根中的完整插件配置后重试")
    raw_plugins, _plugins_snapshot = read_once(plugins_js)
    plugins = parse_plugins(decode_text(raw_plugins, plugins_js), str(plugins_js))
    active_plugins = [plugin for plugin in plugins if plugin.status]
    documents: dict[str, JsonValue] = {}
    relative_files: dict[str, str] = {}
    unresolved_documents: list[tuple[Path, str]] = []
    canonical_maps = {
        path.name: number for path in data_paths if (number := canonical_map_number(path.name)) is not None
    }
    for path in data_paths:
        raw, snapshot = read_once(path)
        relative_files[path.name] = snapshot.relative_path
        try:
            documents[path.name] = parse_json_text(decode_text(raw, path), str(path))
        except ToolError as error:
            strict = path.name in STANDARD_DATA_FILES or path.name in canonical_maps
            if strict:
                raise
            unresolved_documents.append((path, error.reason))

    locations = builtin_database_locations(documents, relative_files, game.engine)
    locations.extend(builtin_system_locations(documents, relative_files))
    events = list(event_lists(documents, canonical_maps))
    locations.extend(builtin_event_locations(events, documents, relative_files, game.engine))
    event_parameter_paths = {
        (
            event.source_file,
            (*event.command_steps, "parameters", parameter, *leaf.path),
        )
        for event in events
        for parameter, value in enumerate(event.parameters)
        for leaf in iter_string_leaves(value)
        if not is_structural_blank(leaf.value)
    }

    # Data 根 JSON 只读一次；Builtin 位置和事件参数分别由它们的语义扫描负责。
    for path in data_paths:
        root = documents.get(path.name)
        if root is None:
            continue
        canonical_map = path.name in canonical_maps
        for leaf in iter_string_leaves(root):
            if is_structural_blank(leaf.value):
                locations.append(
                    LocationFact(
                        source=f"data/{path.name}",
                        location=f"{path.name}:{actual_path(leaf.path)}",
                        source_text=leaf.value,
                        classification="structural_whitespace",
                        physical_file=relative_files[path.name],
                        json_path=leaf.path,
                        decode_positions=leaf.decode_positions,
                        roles={"structure"},
                    )
                )
                continue
            if (path.name, leaf.path) in event_parameter_paths:
                # 非空事件参数由 command scanner 建立唯一 Rules/Manual 投影；
                # 空白事实仍由 data 扫描保留。
                continue
            if is_builtin_data_path(path.name, leaf.path, canonical_map=canonical_map):
                continue
            resource = classify_resource_reference(leaf.path, leaf.value)
            location = f"{path.name}:{actual_path(leaf.path)}"
            if resource is not None:
                locations.append(
                    LocationFact(
                        source=f"data/{path.name}",
                        location=location,
                        source_text=leaf.value,
                        classification="resource_reference",
                        physical_file=relative_files[path.name],
                        json_path=leaf.path,
                        decode_positions=leaf.decode_positions,
                        resource={
                            "basis": resource.basis,
                            "resource_kind": resource.resource_kind,
                        },
                        roles={"resource"},
                    )
                )
                continue
            rule, rule_evidence = rule_proposal(
                {"file": path.name, "path": normalized_path(leaf.path)},
                leaf.value,
            )
            locations.append(
                LocationFact(
                    source=f"data/{path.name}",
                    location=location,
                    source_text=leaf.value,
                    classification="review",
                    physical_file=relative_files[path.name],
                    json_path=leaf.path,
                    decode_positions=leaf.decode_positions,
                    rule=rule,
                    expected_manual_id=rule_manual_id(
                        leaf.path,
                        leaf.decode_positions,
                        source_file=path.name,
                    ),
                    manual_type="fixed",
                    roles={"unknown"},
                    evidence=[*rule_evidence, *lexical_suggestion(leaf.value)],
                )
            )

    # 事件命令参数具有独立 Rules 来源；跳过 Builtin 已拥有的精确槽。
    for event in events:
        for parameter, value in enumerate(event.parameters):
            for leaf in iter_string_leaves(value):
                physical_path = (*event.command_steps, "parameters", parameter, *leaf.path)
                decode_positions = tuple(
                    len(event.command_steps) + 2 + position for position in leaf.decode_positions
                )
                source = f"event-command:{event.code}:parameter:{parameter}"
                location = f"{event.source_file}:{actual_path(physical_path)}"
                if is_structural_blank(leaf.value):
                    # data 根扫描已经按同一物理路径保留这个空白叶子。
                    continue
                if is_builtin_command_leaf(event, parameter, leaf.path, game.engine):
                    locations.append(
                        LocationFact(
                            source=source,
                            location=location,
                            source_text=leaf.value,
                            classification="builtin",
                            physical_file=relative_files[event.source_file],
                            json_path=physical_path,
                            decode_positions=decode_positions,
                            roles={"builtin_member"},
                            evidence=[
                                {
                                    "kind": "builtin_event_member",
                                    "analysis_status": "confirmed",
                                }
                            ],
                        )
                    )
                    continue
                resource = classify_resource_reference(
                    leaf.path,
                    leaf.value,
                    command_code=event.code,
                    parameter=parameter,
                )
                if resource is not None:
                    locations.append(
                        LocationFact(
                            source=source,
                            location=location,
                            source_text=leaf.value,
                            classification="resource_reference",
                            physical_file=relative_files[event.source_file],
                            json_path=physical_path,
                            decode_positions=decode_positions,
                            resource={
                                "basis": resource.basis,
                                "resource_kind": resource.resource_kind,
                            },
                            roles={"resource"},
                        )
                    )
                    continue
                base_rule: dict[str, JsonValue] = {
                    "code": event.code,
                    "parameter": parameter,
                }
                if leaf.path:
                    base_rule["path"] = normalized_path(leaf.path)
                rule, rule_evidence = rule_proposal(base_rule, leaf.value)
                command_path = (*event.command_steps, "parameters", parameter, *leaf.path)
                locations.append(
                    LocationFact(
                        source=source,
                        location=location,
                        source_text=leaf.value,
                        classification="review",
                        physical_file=relative_files[event.source_file],
                        json_path=physical_path,
                        decode_positions=decode_positions,
                        rule=rule,
                        expected_manual_id=rule_manual_id(
                            command_path,
                            decode_positions,
                            source_file=event.source_file,
                            command_group_steps=event.command_steps,
                            command_path_has_index=any(isinstance(step, int) for step in leaf.path),
                        ),
                        manual_type="fixed",
                        roles={"unknown"},
                        evidence=[*rule_evidence, *lexical_suggestion(leaf.value)],
                    )
                )

    _code_scans, scanned_code_paths = scan_code_sources(
        game,
        game_root,
        files,
        plugins,
        plugins_js,
        locations,
        read_once,
    )
    for path in files:
        if path.resolve(strict=True) in scanned_code_paths or path in data_paths or path == plugins_js:
            continue
        if path.suffix.lower() not in _EXTERNAL_TEXT_SUFFIXES:
            continue
        raw, snapshot = read_once(path)
        try:
            text = raw.decode("utf-8-sig")
        except UnicodeDecodeError as error:
            locations.append(
                LocationFact(
                    source=snapshot.relative_path,
                    location=snapshot.relative_path,
                    source_text="",
                    classification="review",
                    physical_file=snapshot.relative_path,
                    roles={"unknown"},
                    evidence=[
                        {
                            "kind": "unsupported_encoding",
                            "byte_position": error.start,
                            "analysis_status": "unknown",
                        }
                    ],
                    generic_kind="unsupported_encoding",
                )
            )
            continue
        if path.suffix.lower() == ".json":
            try:
                external_json = parse_json_text(text, str(path))
            except ToolError as error:
                locations.append(
                    LocationFact(
                        source=snapshot.relative_path,
                        location=snapshot.relative_path,
                        source_text="",
                        classification="review",
                        physical_file=snapshot.relative_path,
                        roles={"unknown"},
                        evidence=[
                            {
                                "kind": "unparsed_external_json",
                                "reason": error.reason,
                                "analysis_status": "unknown",
                            }
                        ],
                        generic_kind="unparsed_source",
                    )
                )
                continue
            for leaf in iter_string_leaves(external_json):
                json_location = f"{snapshot.relative_path}:{actual_path(leaf.path)}"
                if is_structural_blank(leaf.value):
                    locations.append(
                        LocationFact(
                            source=snapshot.relative_path,
                            location=json_location,
                            source_text=leaf.value,
                            classification="structural_whitespace",
                            physical_file=snapshot.relative_path,
                            json_path=leaf.path,
                            decode_positions=leaf.decode_positions,
                            roles={"structure"},
                            generic_kind="json_string",
                        )
                    )
                    continue
                resource = classify_resource_reference(leaf.path, leaf.value)
                if resource is not None:
                    locations.append(
                        LocationFact(
                            source=snapshot.relative_path,
                            location=json_location,
                            source_text=leaf.value,
                            classification="resource_reference",
                            physical_file=snapshot.relative_path,
                            json_path=leaf.path,
                            decode_positions=leaf.decode_positions,
                            roles={"resource"},
                            resource={
                                "basis": resource.basis,
                                "resource_kind": resource.resource_kind,
                            },
                            generic_kind="json_string",
                        )
                    )
                    continue
                locations.append(
                    LocationFact(
                        source=snapshot.relative_path,
                        location=json_location,
                        source_text=leaf.value,
                        classification="review",
                        physical_file=snapshot.relative_path,
                        json_path=leaf.path,
                        decode_positions=leaf.decode_positions,
                        roles={"unknown"},
                        evidence=[
                            {
                                "kind": "external_json_string",
                                "active_runtime_consumer": "unconfirmed",
                                "analysis_status": "unknown",
                            },
                            *lexical_suggestion(leaf.value),
                        ],
                        generic_kind="json_string",
                        generic_locator={
                            "path": list(leaf.path),
                            "decode_positions": list(leaf.decode_positions),
                        },
                    )
                )
            continue
        for line_number, line in enumerate(split_rpg_text_lines(text), start=1):
            if is_structural_blank(line):
                locations.append(
                    LocationFact(
                        source=snapshot.relative_path,
                        location=f"{snapshot.relative_path}:line{line_number}",
                        source_text=line,
                        classification="structural_whitespace",
                        physical_file=snapshot.relative_path,
                        roles={"structure"},
                        generic_kind="plain_text_line",
                        generic_locator={"line": line_number},
                    )
                )
                continue
            locations.append(
                LocationFact(
                    source=snapshot.relative_path,
                    location=f"{snapshot.relative_path}:line{line_number}",
                    source_text=line,
                    classification="review",
                    physical_file=snapshot.relative_path,
                    roles={"unknown"},
                    evidence=[
                        {
                            "kind": "external_text_source",
                            "active_runtime_consumer": "unconfirmed",
                            "analysis_status": "unknown",
                        },
                        *lexical_suggestion(line),
                    ],
                    generic_kind="plain_text_line",
                    generic_locator={"line": line_number},
                )
            )

    for path, reason in unresolved_documents:
        relative = path.relative_to(game_root).as_posix()
        locations.append(
            LocationFact(
                source=relative,
                location=relative,
                source_text="",
                classification="review",
                physical_file=relative,
                roles={"unknown"},
                evidence=[
                    {
                        "kind": "unparsed_custom_data_json",
                        "reason": reason,
                        "analysis_status": "unknown",
                    }
                ],
                generic_kind="unparsed_source",
            )
        )

    for number, fact in enumerate(locations, start=1):
        fact.candidate_id = f"location-{number:06d}"
    review_groups = build_review_groups(locations)
    review_packet_count = len(
        {packet_id for fact in locations if isinstance((packet_id := fact.review_packet_id), str)}
    )
    elapsed_ms = round((time.perf_counter() - started) * 1000)
    location_values = tuple(fact.json() for fact in locations)
    manifest_values = [snapshots[key].json() for key in sorted(snapshots)]
    selection_paths = sorted(
        path.relative_to(game_root).as_posix()
        for path in files
        if path == plugins_js
        or (path.parent == data_root and path.suffix.lower() == ".json")
        or path.suffix.lower() in _EXTERNAL_TEXT_SUFFIXES
    )
    source_baseline: dict[str, JsonValue] = {
        "scope": "本次调查实际读取的 RPG Maker 数据、活动代码和文本容器",
        "files": manifest_values,
        "selection": {
            "data_directory": data_root.relative_to(game_root).as_posix(),
            "plugins_file": plugins_js.relative_to(game_root).as_posix(),
            "external_suffixes": sorted(_EXTERNAL_TEXT_SUFFIXES),
            "paths": selection_paths,
        },
    }
    review_candidate_count = sum(fact.classification == "review" for fact in locations)
    summary: dict[str, JsonValue] = {
        "engine": game.engine,
        "game_root": str(game_root),
        "content_root": str(game.content_root),
        "active_plugins": len(active_plugins),
        "canonical_maps": len(canonical_maps),
        "locations": len(locations),
        "builtin_locations": sum(fact.classification == "builtin" for fact in locations),
        "resource_references": sum(fact.classification == "resource_reference" for fact in locations),
        "structural_whitespace": sum(fact.classification == "structural_whitespace" for fact in locations),
        "review_candidates": review_candidate_count,
        "review_packets": review_packet_count,
        "review_groups": len(review_groups),
        "runtime_complete": False,
        "agent_work_metrics": {
            "game_tree_scans": 1,
            "files_enumerated": len(files),
            "files_read": files_read,
            "file_read_operations": files_read,
            "bytes_read": sum(snapshot.bytes_count for snapshot in snapshots.values()),
            "local_command_elapsed_ms": elapsed_ms,
            "review_groups": len(review_groups),
            "review_packets": review_packet_count,
            "explicit_decisions_required": len(review_groups),
            "handwritten_rule_objects_required": 0,
            "external_request_wait_ms": 0,
        },
    }
    return SurveyBundle(
        summary=summary,
        locations=location_values,
        review_groups=tuple(review_groups),
        source_baseline=source_baseline,
    )


def json_lines(values: Iterable[Mapping[str, JsonValue]]) -> str:
    return "".join(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n" for value in values)
