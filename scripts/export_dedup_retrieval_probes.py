#!/usr/bin/env python3
"""Export counterfactual positive probes from robustly linked entries.

Each probe partitions one existing entry's source records into two disjoint
pseudo-entries. These are retrieval ground truth only, never annotation tasks or
production merge suggestions.
"""

from __future__ import annotations

import argparse
import difflib
from pathlib import Path

from export_dedup_calibration import (
    ENTRY_TYPES,
    Snapshot,
    canonical_json,
    normalize_name,
    script_relation,
    sha256_file,
    sha256_value,
)


def names(view: dict) -> set[str]:
    return {
        normalize_name(str(fact["value"].get("name", "")))
        for record in view["records"]
        for fact in record["facts"]
        if fact["field"] == "alias" and isinstance(fact["value"], dict)
    } - {""}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, default=Path("data/dedup-corpus-v3.db"))
    parser.add_argument(
        "--output", type=Path, default=Path("data/dedup-entry-retrieval-probes-v4.jsonl")
    )
    parser.add_argument("--max-records-per-view", type=int, default=20)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    snapshot_id = sha256_file(args.db)
    snapshot = Snapshot(args.db, args.max_records_per_view)
    probes = []
    try:
        for entry_id in sorted(snapshot.entry_types):
            if snapshot.entry_types[entry_id] not in ENTRY_TYPES:
                continue
            pairs = [
                (str(row["source"]), str(row["identifier"]))
                for row in snapshot.sources_by_entry.get(entry_id, [])[: args.max_records_per_view]
            ]
            if len(pairs) < 2:
                continue
            partitions: list[tuple[str, list[tuple[str, str]], list[tuple[str, str]]]] = []
            for source in sorted({pair[0] for pair in pairs}):
                left = [pair for pair in pairs if pair[0] == source]
                right = [pair for pair in pairs if pair[0] != source]
                if left and right:
                    partitions.append((f"source:{source}", left, right))
            if pairs[::2] and pairs[1::2]:
                partitions.append(("alternating", pairs[::2], pairs[1::2]))

            eligible = []
            for partition_name, left_pairs, right_pairs in partitions:
                left = snapshot.view(entry_id, left_pairs)
                right = snapshot.view(entry_id, right_pairs)
                left_names, right_names = names(left), names(right)
                if not left_names or not right_names:
                    continue
                similarity = max(
                    difflib.SequenceMatcher(None, a, b).ratio()
                    for a in left_names
                    for b in right_names
                )
                exact = bool(left_names & right_names)
                relation = script_relation(left["display_title"], right["display_title"])
                challenge = (
                    int(not exact),
                    int(relation == "cjk_latin"),
                    int(relation == "mixed"),
                    -similarity,
                    min(len(left_pairs), len(right_pairs)),
                    partition_name,
                )
                eligible.append(
                    (challenge, partition_name, left_pairs, right_pairs, left, right, exact, relation, similarity)
                )
            if not eligible:
                continue
            _, partition_name, left_pairs, right_pairs, left, right, exact, relation, similarity = max(
                eligible, key=lambda row: row[0]
            )
            probes.append(
                {
                    "record_type": "retrieval_probe",
                    "probe_version": "dedup-retrieval-probe/1",
                    "probe_id": sha256_value([snapshot_id, entry_id, left["view_id"], right["view_id"]]),
                    "snapshot_id": snapshot_id,
                    "ground_truth": "same_identity",
                    "original_entry_id": entry_id,
                    "entry_type": snapshot.entry_types[entry_id],
                    "script_relation": relation,
                    "exact_alias_overlap": exact,
                    "best_name_similarity": similarity,
                    "partition": partition_name,
                    "left_source_count": len(left_pairs),
                    "right_source_count": len(right_pairs),
                    "left": left,
                    "right": right,
                }
            )
    finally:
        snapshot.close()

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8") as handle:
        for probe in probes:
            handle.write(canonical_json(probe) + "\n")
    nonexact = sum(not probe["exact_alias_overlap"] for probe in probes)
    print(f"probes={len(probes)} nonexact={nonexact} snapshot={snapshot_id} output={args.output}")


if __name__ == "__main__":
    main()
