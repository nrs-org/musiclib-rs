#!/usr/bin/env python3
"""Export a blind, positive-enriched review batch from PoC candidates.

This is an active-learning/calibration batch, not a prevalence sample. Scores,
retrieval channels, and selection metadata remain private.
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
from pathlib import Path
from typing import Any, Callable

from export_dedup_calibration import (
    Snapshot,
    blind_projection,
    make_candidate,
    script_relation,
    sha256_file,
    write_jsonl,
)


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    with path.open(encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def excluded_pairs(paths: list[Path]) -> set[tuple[int, int]]:
    result = set()
    for path in paths:
        for task in load_jsonl(path):
            ids = task.get("hidden", {}).get("current_entry_ids", [])
            if len(ids) == 2 and ids[0] != ids[1]:
                result.add(tuple(sorted(map(int, ids))))
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, default=Path("data/dedup-corpus-v3.db"))
    parser.add_argument(
        "--corpus-views",
        type=Path,
        default=Path("data/dedup-entry-pilot-v3.corpus-views.jsonl"),
    )
    parser.add_argument(
        "--report",
        type=Path,
        default=Path("data/dedup-entry-pilot-v3-poc-semantic/report.json"),
    )
    parser.add_argument(
        "--exclude-task-ledger",
        type=Path,
        action="append",
        default=[Path("data/dedup-entry-pilot-v3.private.jsonl")],
    )
    parser.add_argument(
        "--private-out",
        type=Path,
        default=Path("data/dedup-entry-active-v4.private.jsonl"),
    )
    parser.add_argument(
        "--blind-out",
        type=Path,
        default=Path("data/dedup-entry-active-v4.blind.jsonl"),
    )
    parser.add_argument("--per-stratum", type=int, default=20)
    parser.add_argument("--max-entry-uses", type=int, default=2)
    parser.add_argument("--max-records-per-view", type=int, default=20)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.per_stratum < 1 or args.max_entry_uses < 1:
        raise ValueError("quotas must be positive")

    corpus_rows = load_jsonl(args.corpus_views)
    view_to_entry = {row["view"]["view_id"]: int(row["entry_id"]) for row in corpus_rows}
    corpus_snapshot_ids = {row["snapshot_id"] for row in corpus_rows}
    snapshot_id = sha256_file(args.db)
    if corpus_snapshot_ids != {snapshot_id}:
        raise ValueError("corpus view export and database snapshot differ")

    report = json.loads(args.report.read_text(encoding="utf-8"))
    rows = [row for row in report["candidates"] if row["gold"] == "unlabeled"]
    excluded = excluded_pairs(args.exclude_task_ledger)

    def has(channel: str) -> Callable[[dict[str, Any]], bool]:
        return lambda row: channel in row["channels"] and "exact_name" not in row["channels"]

    strata: list[tuple[str, Callable[[dict[str, Any]], bool], Callable[[dict[str, Any]], float]]] = [
        ("exact_confuser", lambda row: "exact_name" in row["channels"], lambda row: row["probability_same"]),
        ("romanized_nonexact", has("romanized_name"), lambda row: row["features"]["romanized_similarity"]),
        ("duration_credit_nonexact", has("duration_credit"), lambda row: row["probability_same"]),
        ("tracklist_overlap_nonexact", has("tracklist_overlap"), lambda row: row["features"]["tracklist_jaccard"]),
        ("semantic_only", lambda row: row["channels"] == ["semantic_ann"], lambda row: row["features"]["semantic_similarity"]),
        ("model_boundary_nonexact", lambda row: "exact_name" not in row["channels"], lambda row: -abs(row["probability_same"] - 0.5)),
    ]

    selected: list[tuple[str, dict[str, Any], tuple[int, int]]] = []
    selected_pairs: set[tuple[int, int]] = set()
    entry_uses: collections.Counter[int] = collections.Counter()
    eligible_counts: dict[str, int] = {}
    for stratum, predicate, score in strata:
        eligible: list[tuple[float, str, dict[str, Any], tuple[int, int]]] = []
        for row in rows:
            if not predicate(row):
                continue
            try:
                pair = tuple(sorted((view_to_entry[row["left_view_id"]], view_to_entry[row["right_view_id"]])))
            except KeyError:
                continue
            if pair in excluded:
                continue
            eligible.append((score(row), row["short_id"], row, pair))
        eligible.sort(key=lambda item: (item[0], item[1]), reverse=True)
        eligible_counts[stratum] = len(eligible)
        for _, _, row, pair in eligible:
            if len([item for item in selected if item[0] == stratum]) >= args.per_stratum:
                break
            if pair in selected_pairs or any(entry_uses[entry_id] >= args.max_entry_uses for entry_id in pair):
                continue
            selected.append((stratum, row, pair))
            selected_pairs.add(pair)
            entry_uses.update(pair)

    created_at = dt.datetime.now(tz=dt.timezone.utc).isoformat().replace("+00:00", "Z")
    snapshot = Snapshot(args.db, args.max_records_per_view)
    tasks = []
    selected_counts = collections.Counter(stratum for stratum, _, _ in selected)
    try:
        for stratum, row, pair in selected:
            left, right = snapshot.view(pair[0]), snapshot.view(pair[1])
            hidden = {
                "sampling_frame": "active_learning",
                "stratum": f"{left['entry_type']}/{script_relation(left['display_title'], right['display_title'])}/{stratum}",
                "eligible_count": eligible_counts[stratum],
                "selected_count": selected_counts[stratum],
                "selection_method": "ranked_positive_enrichment_with_entity_cap",
                "inclusion_probability": None,
                "production_candidate": None,
                "candidate_set_version": sha256_file(args.report),
                "proposers": [
                    {
                        "name": channel,
                        "version": "dedup-poc-entry-v0",
                        "rank": None,
                        "score": row["probability_same"],
                        "score_name": "model_probability_same",
                    }
                    for channel in row["channels"]
                ],
                "masked_links": [],
                "current_entry_ids": list(pair),
                "split_group_keys": [f"local-entry:{pair[0]}", f"local-entry:{pair[1]}"],
                "partition": "train",
            }
            task = make_candidate(snapshot_id, created_at, left, right, hidden)
            if task is not None:
                tasks.append(task)
    finally:
        snapshot.close()

    tasks.sort(key=lambda task: (task["hidden"]["stratum"], task["item_id"]))
    write_jsonl(args.private_out, tasks)
    write_jsonl(args.blind_out, (blind_projection(task) for task in tasks))
    print(f"tasks={len(tasks)} private={args.private_out} blind={args.blind_out}")
    print("strata=" + json.dumps(dict(sorted(selected_counts.items())), sort_keys=True))


if __name__ == "__main__":
    main()
