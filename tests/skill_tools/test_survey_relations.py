from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "_shared"))
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

from att_toolbox.survey_dsu import DisjointSet
from att_toolbox.survey_model import LocationFact
from att_toolbox.survey_relations import _union_related_groups, review_groups


class DisjointSetTests(unittest.TestCase):
    def test_long_union_chain_uses_iterative_find(self) -> None:
        size = 5_000
        groups = DisjointSet(size)
        for index in range(1, size):
            groups.union(index, index - 1)

        expected = groups.find(size - 1)
        self.assertTrue(all(groups.find(index) == expected for index in range(size)))


class SurveyRelationTests(unittest.TestCase):
    def test_single_large_domain_does_not_materialize_internal_token_pairs(self) -> None:
        groups = DisjointSet(1)

        _union_related_groups(
            groups,
            [{f"token-{index}" for index in range(10_000)}],
            [set()],
            [{"data/Large.json"}],
            [{"data/Large.json:packet"}],
        )

        self.assertEqual(groups.find(0), 0)

    def test_many_groups_sharing_only_one_token_keep_linear_memory_shape(self) -> None:
        size = 5_000
        groups = DisjointSet(size)

        _union_related_groups(
            groups,
            [{"common", f"unique-{index}"} for index in range(size)],
            [set() for _index in range(size)],
            [{"data/Large.json"} for _index in range(size)],
            [{"data/Large.json:packet"} for _index in range(size)],
        )

        self.assertEqual(len({groups.find(index) for index in range(size)}), size)

    def test_many_wide_groups_sharing_one_frequent_token_do_not_materialize_group_pairs(self) -> None:
        size = 2_000
        groups = DisjointSet(size)

        _union_related_groups(
            groups,
            [{"common", *(f"unique-{index}-{token}" for token in range(100))} for index in range(size)],
            [set() for _index in range(size)],
            [{"data/Large.json"} for _index in range(size)],
            [{"data/Large.json:packet"} for _index in range(size)],
        )

        self.assertEqual(len({groups.find(index) for index in range(size)}), size)

    def test_one_group_with_many_sources_and_packets_does_not_materialize_their_product(self) -> None:
        groups = DisjointSet(1)

        _union_related_groups(
            groups,
            [{"domain-a", "domain-b"}],
            [set()],
            [{f"source-{index}" for index in range(2_000)}],
            [{f"packet-{index}" for index in range(2_000)}],
        )

        self.assertEqual(groups.find(0), 0)

    def test_one_domain_and_many_references_do_not_scan_reference_pairs(self) -> None:
        size = 5_000
        groups = DisjointSet(size)

        _union_related_groups(
            groups,
            [{"shared", "domain-only"}, *(set() for _index in range(size - 1))],
            [set(), *({"shared"} for _index in range(size - 1))],
            [{"data/Large.json"} for _index in range(size)],
            [{"data/Large.json:packet"} for _index in range(size)],
        )

        self.assertEqual(len({groups.find(index) for index in range(size)}), 1)

    def test_same_domains_in_disjoint_scopes_do_not_scan_cross_scope_pairs(self) -> None:
        size = 5_000
        groups = DisjointSet(size)

        _union_related_groups(
            groups,
            [{"domain-a", "domain-b"} for _index in range(size)],
            [set() for _index in range(size)],
            [{f"source-{index}"} for index in range(size)],
            [{f"packet-{index}"} for index in range(size)],
        )

        self.assertEqual(len({groups.find(index) for index in range(size)}), size)

    def test_one_group_wide_in_tokens_sources_and_packets_does_not_materialize_products(self) -> None:
        groups = DisjointSet(1)

        _union_related_groups(
            groups,
            [{f"domain-{index}" for index in range(2_000)}],
            [set()],
            [{f"source-{index}" for index in range(2_000)}],
            [{f"packet-{index}" for index in range(2_000)}],
        )

        self.assertEqual(groups.find(0), 0)

    def test_large_token_chain_forms_one_relation_group(self) -> None:
        locations: list[LocationFact] = []
        for index in range(1_200):
            locations.append(
                LocationFact(
                    source="data/Relations.json",
                    location=f"Relations.json:item{index + 1}",
                    source_text=f"[token{index}],token{index},token{index + 1}",
                    classification="review",
                    physical_file="data/Relations.json",
                    rule={"file": "Relations.json", "path": f"[].field{index}"},
                    roles={"unknown"},
                    candidate_id=f"location-{index + 1:06d}",
                )
            )

        groups = review_groups(locations)

        self.assertEqual(len(groups), 1)
        self.assertEqual(groups[0]["kind"], "relation_group")
        self.assertEqual(groups[0]["location_count"], len(locations))

    def test_token_overlap_does_not_cross_source_or_packet_boundaries(self) -> None:
        locations = [
            LocationFact(
                source=f"data/Source{index}.json",
                location=f"Source{index}.json:item",
                source_text="[shared],shared,other",
                classification="review",
                physical_file=f"data/Source{index}.json",
                rule={"file": f"Source{index}.json", "path": "[].text"},
                roles={"unknown"},
                candidate_id=f"location-{index + 1:06d}",
            )
            for index in range(2)
        ]

        groups = review_groups(locations)

        self.assertEqual(len(groups), 2)


if __name__ == "__main__":
    unittest.main()
