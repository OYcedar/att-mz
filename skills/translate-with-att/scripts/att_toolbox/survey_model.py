"""调查位置、基线和输出模型。"""

from __future__ import annotations

from dataclasses import dataclass, field

from att_skill_tools import JsonValue


@dataclass(frozen=True, slots=True)
class FileSnapshot:
    relative_path: str
    bytes_count: int
    sha256: str

    def json(self) -> dict[str, JsonValue]:
        return {
            "path": self.relative_path,
            "bytes": self.bytes_count,
            "sha256": self.sha256,
        }


@dataclass(slots=True)
class LocationFact:
    source: str
    location: str
    source_text: str
    classification: str
    physical_file: str
    json_path: tuple[str | int, ...] = ()
    decode_positions: tuple[int, ...] = ()
    rule: dict[str, JsonValue] | None = None
    expected_manual_id: str | None = None
    control_contract: dict[str, JsonValue] | None = None
    roles: set[str] = field(default_factory=set)
    evidence: list[dict[str, JsonValue]] = field(default_factory=list)
    resource: dict[str, JsonValue] | None = None
    generic_kind: str | None = None
    generic_locator: dict[str, JsonValue] | None = None
    dialogue_first_line: str | None = None
    review_packet_id: str | None = None
    review_group_id: str | None = None
    candidate_id: str = ""

    def json(self) -> dict[str, JsonValue]:
        value: dict[str, JsonValue] = {
            "candidate_id": self.candidate_id,
            "source": self.source,
            "location": self.location,
            "source_text": self.source_text,
            "classification": self.classification,
            "physical_file": self.physical_file,
            "json_path": list(self.json_path),
            "decode_positions": list(self.decode_positions),
            "roles": sorted(self.roles),
            "consumer_evidence": self.evidence,
        }
        if self.rule is not None:
            value["rule"] = self.rule
        if self.expected_manual_id is not None:
            value["expected_manual_id"] = self.expected_manual_id
        if self.control_contract is not None:
            value["control_contract"] = self.control_contract
        if self.resource is not None:
            value["resource_reference"] = self.resource
        if self.generic_kind is not None:
            value["generic_kind"] = self.generic_kind
        if self.generic_locator is not None:
            value["generic_locator"] = self.generic_locator
        if self.dialogue_first_line is not None:
            value["dialogue_first_line"] = self.dialogue_first_line
        if self.review_packet_id is not None:
            value["review_packet_id"] = self.review_packet_id
        if self.review_group_id is not None:
            value["review_group_id"] = self.review_group_id
        return value


@dataclass(frozen=True, slots=True)
class EventFact:
    source_file: str
    source_kind: str
    list_steps: tuple[str | int, ...]
    command_index: int
    code: int
    parameters: tuple[JsonValue, ...]

    @property
    def command_steps(self) -> tuple[str | int, ...]:
        return (*self.list_steps, self.command_index)


@dataclass(frozen=True, slots=True)
class SurveyBundle:
    summary: dict[str, JsonValue]
    locations: tuple[dict[str, JsonValue], ...]
    review_groups: tuple[dict[str, JsonValue], ...]
    source_baseline: dict[str, JsonValue]
