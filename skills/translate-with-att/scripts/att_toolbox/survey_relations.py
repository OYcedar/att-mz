"""调查候选的关系组建立。"""

from __future__ import annotations

import re
from collections import defaultdict
from collections.abc import Iterable, Sequence

from att_skill_tools import JsonValue

from .survey_dsu import DisjointSet
from .survey_identity import rule_key, source_name_for_rule
from .survey_model import LocationFact

_SIMPLE_DOMAIN_TOKEN = re.compile(r"[^,;|\r\n]{1,80}")
_CANONICAL_MAP = re.compile(r"Map[0-9]+\.json\Z", re.IGNORECASE)
_PLUGIN_INDEX = re.compile(r"\Aplugins\.js:plugin([0-9]+):")


def _role_key(fact: LocationFact) -> str:
    display = "display_candidate" in fact.roles
    protocol = "protocol_candidate" in fact.roles
    if display and protocol:
        return "display_protocol"
    if display:
        return "display"
    if protocol:
        return "protocol"
    return "unknown"


def _consumer_fingerprint(fact: LocationFact) -> str:
    values: set[str] = set()
    for evidence in fact.evidence:
        raw = evidence.get("consumer_fingerprint")
        if not isinstance(raw, list):
            continue
        values.update(value for value in raw if isinstance(value, str) and value)
    return "+".join(sorted(values)) or "unknown"


def _semantic_field_family(path: object) -> str:
    if not isinstance(path, str) or not path:
        return "root"
    tail = path.rsplit(".", 1)[-1]
    return re.sub(r"\[\]", "[]", tail)


def _review_packet_key(fact: LocationFact) -> str:
    """把相关事实装成阅读 packet；packet 自身不充当 owner 决定。"""

    if fact.rule is not None:
        plugin = fact.rule.get("plugin")
        if isinstance(plugin, str):
            match = _PLUGIN_INDEX.match(fact.location)
            index = match.group(1) if match is not None else "unknown"
            return f"rules-plugin:{index}:{plugin}"
        file_name = fact.rule.get("file")
        if isinstance(file_name, str) and _CANONICAL_MAP.fullmatch(file_name) is not None:
            path = fact.rule.get("path")
            family = path if isinstance(path, str) else "root"
            return f"rules-canonical-map:{family}"
        return f"rules-source:{source_name_for_rule(fact.rule)}"
    role = "display" if "display_candidate" in fact.roles else "unknown"
    if fact.generic_kind == "javascript_literal":
        return f"generic:active-javascript:{role}"
    if re.fullmatch(r"www/img/tilesets/[^/]+\.txt", fact.source, re.IGNORECASE):
        return f"generic:tileset-text-container:{role}"
    if re.fullmatch(r"www/logs/[^/]+\.txt", fact.source, re.IGNORECASE):
        return f"generic:runtime-log-container:{role}"
    return f"generic:{fact.source}:{fact.generic_kind}:{role}"


def _decision_group_key(fact: LocationFact) -> str:
    """只有消费者关系和角色能够共存的事实才进入同一决定组。"""

    role = _role_key(fact)
    fingerprint = _consumer_fingerprint(fact)
    if fact.rule is not None:
        plugin = fact.rule.get("plugin")
        if isinstance(plugin, str):
            match = _PLUGIN_INDEX.match(fact.location)
            index = match.group(1) if match is not None else "unknown"
            if fingerprint.startswith("display:"):
                return f"rules-plugin-consumer:{fingerprint}:display"
            if fingerprint.startswith("protocol:"):
                top_parameter = (
                    str(fact.json_path[2])
                    if len(fact.json_path) > 2 and fact.json_path[1] == "parameters"
                    else "unknown"
                )
                return f"rules-plugin:{index}:{top_parameter}:protocol:{fingerprint}"
            return f"rules-plugin:{index}:{role}:{fingerprint}"
        file_name = fact.rule.get("file")
        path = fact.rule.get("path")
        family = _semantic_field_family(path)
        if isinstance(file_name, str) and _CANONICAL_MAP.fullmatch(file_name) is not None:
            full_family = path if isinstance(path, str) else "root"
            return f"rules-canonical-map:{full_family}:{role}"
        return f"rules-source:{source_name_for_rule(fact.rule)}:{family}:{role}"
    if fact.generic_kind == "javascript_literal":
        if fingerprint == "unknown":
            return f"generic-javascript:{fact.source}:unknown"
        return f"generic-javascript-consumer:{fingerprint}:{role}"
    return f"generic-source:{fact.source}:{fact.generic_kind}:{role}"


def _representative_examples(facts: Sequence[LocationFact], limit: int = 5) -> list[dict[str, JsonValue]]:
    """只保留少量不同正文，完整位置事实留在 locations.jsonl。"""

    output: list[dict[str, JsonValue]] = []
    seen_text: set[str] = set()
    for fact in facts:
        if fact.source_text in seen_text:
            continue
        seen_text.add(fact.source_text)
        truncated = len(fact.source_text) > 160
        output.append(
            {
                "candidate_id": fact.candidate_id,
                "location": fact.location,
                "source_text_preview": (fact.source_text if not truncated else fact.source_text[:160]),
                "source_text_characters": len(fact.source_text),
                "truncated": truncated,
            }
        )
        if len(output) == limit:
            break
    return output


def _evidence_summaries(facts: Sequence[LocationFact], limit: int = 5) -> list[JsonValue]:
    """汇总消费者证据种类，不把每个位置的机器明细复制给 Agent。"""

    summaries: dict[tuple[str, str], dict[str, JsonValue]] = {}
    for fact in facts:
        for evidence in fact.evidence:
            kind = evidence.get("kind")
            status = evidence.get("analysis_status")
            if not isinstance(kind, str):
                continue
            status_text = status if isinstance(status, str) else "unknown"
            key = (kind, status_text)
            summary = summaries.setdefault(
                key,
                {
                    "kind": kind,
                    "analysis_status": status_text,
                    "locations": 0,
                },
            )
            current = summary.get("locations")
            summary["locations"] = (
                current + 1 if isinstance(current, int) and not isinstance(current, bool) else 1
            )
    return [
        f"{summary['kind']}:{summary['analysis_status']}:{summary['locations']}"
        for key in sorted(summaries)[:limit]
        if (summary := summaries[key])
    ]


def _domain_tokens(values: Iterable[str]) -> set[str]:
    tokens: set[str] = set()
    for value in values:
        stripped = value.strip()
        delimiter = next((item for item in (",", ";", "|") if item in stripped), None)
        if delimiter is None:
            continue
        parts = [part.strip() for part in stripped.split(delimiter)]
        if len(parts) < 2 or any(not part or _SIMPLE_DOMAIN_TOKEN.fullmatch(part) is None for part in parts):
            continue
        tokens.update(part.casefold() for part in parts)
    return tokens


def _reference_tokens(values: Iterable[str]) -> set[str]:
    output: set[str] = set()
    for value in values:
        stripped = value.strip()
        if 0 < len(stripped) <= 80:
            output.add(stripped.casefold())
        match = re.match(r"\[([^\]\r\n]{1,80})\]", stripped)
        if match is not None:
            output.add(match.group(1).strip().casefold())
    return output


def _same_source_domain_relation(facts: Sequence[LocationFact]) -> bool:
    by_source: dict[str, list[str]] = defaultdict(list)
    for fact in facts:
        by_source[fact.source].append(fact.source_text)
    return any(bool(_domain_tokens(values) & _reference_tokens(values)) for values in by_source.values())


def review_groups(
    locations: Sequence[LocationFact],
) -> list[dict[str, JsonValue]]:
    candidates = [item for item in locations if item.classification == "review"]
    packet_ids = {
        key: f"packet-{number:06d}"
        for number, key in enumerate(sorted({_review_packet_key(fact) for fact in candidates}), start=1)
    }
    grouped: dict[str, list[LocationFact]] = defaultdict(list)
    for fact in candidates:
        grouped[_decision_group_key(fact)].append(fact)
    base = list(grouped.values())
    dsu = DisjointSet(len(base))
    domains = [_domain_tokens(fact.source_text for fact in group) for group in base]
    references = [_reference_tokens(fact.source_text for fact in group) for group in base]
    sources = [{fact.source for fact in group} for group in base]
    packets = [{_review_packet_key(fact) for fact in group} for group in base]
    for left, domain in enumerate(domains):
        if len(domain) < 2:
            continue
        for right, reference in enumerate(references):
            if left == right:
                continue
            same_source = bool(sources[left] & sources[right])
            same_packet = bool(packets[left] & packets[right])
            shared = domain & reference
            matching_domains = len(domains[right]) >= 2 and len(domain & domains[right]) >= 2
            if same_source and same_packet and (shared or matching_domains):
                dsu.union(left, right)
    components: dict[int, list[LocationFact]] = defaultdict(list)
    component_indexes: dict[int, list[int]] = defaultdict(list)
    for index, group in enumerate(base):
        root = dsu.find(index)
        components[root].extend(group)
        component_indexes[root].append(index)

    result: list[dict[str, JsonValue]] = []
    for group_number, root in enumerate(sorted(components), start=1):
        facts = components[root]
        group_id = f"group-{group_number:06d}"
        component_packets = {_review_packet_key(fact) for fact in facts}
        natural_packet_ids = sorted(packet_ids[key] for key in component_packets)
        for fact in facts:
            fact.review_packet_id = packet_ids[_review_packet_key(fact)]
            fact.review_group_id = group_id
        rules_by_key = {rule_key(fact.rule): fact.rule for fact in facts if fact.rule is not None}
        roles = sorted({role for fact in facts for role in fact.roles})
        relation = (
            any(domains[index] for index in component_indexes[root]) and len(component_indexes[root]) > 1
        ) or _same_source_domain_relation(facts)
        if relation:
            roles = sorted(set(roles) | {"display_candidate", "protocol_candidate"})
        analysis_status = "heuristic" if relation or any("candidate" in role for role in roles) else "unknown"
        suggestion = (
            "rules"
            if rules_by_key
            and len(rules_by_key) == len({rule_key(fact.rule) for fact in facts if fact.rule is not None})
            and all(fact.rule is not None for fact in facts)
            else "review"
        )
        rules_capability = (
            "none" if not rules_by_key else "single_shape" if len(rules_by_key) == 1 else "multiple_shapes"
        )
        sources_summary = sorted({fact.source for fact in facts})
        group_value: dict[str, JsonValue] = {
            "group_id": group_id,
            "packet_ids": natural_packet_ids[:5],
            "kind": "relation_group" if relation else "candidate_group",
            "candidate_ids": [fact.candidate_id for fact in facts[:5]],
            "location_count": len(facts),
            "sources": sources_summary[:5],
            "roles": roles,
            "rules_capability": rules_capability,
            "examples": _representative_examples(facts),
            "consumer_evidence": _evidence_summaries(facts),
            "analysis_status": analysis_status,
            "suggestion": suggestion,
            "suggestion_basis": (
                "相同值域在定义列表、默认值或引用项中重复；需 Agent 确认显示与协议关系"
                if relation
                else "规则路径可逆"
                if suggestion == "rules"
                else "静态消费者证据不足"
            ),
        }
        if len(facts) > 5:
            group_value["candidate_ids_complete"] = False
        if len(natural_packet_ids) > 5:
            group_value["packet_count"] = len(natural_packet_ids)
            group_value["packet_ids_complete"] = False
        if len(sources_summary) > 5:
            group_value["source_count"] = len(sources_summary)
            group_value["sources_complete"] = False
        result.append(group_value)
    return result
