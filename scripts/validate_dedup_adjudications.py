#!/usr/bin/env python3
"""Validate append-only dedup adjudications and their input annotations."""

from __future__ import annotations

import argparse
from pathlib import Path
from typing import Any

from validate_dedup_annotations import (
    evidence_ids,
    expected_policy,
    validate_primitive_relations,
)
from validate_dedup_ledger import load_jsonl, schema_validate


def load_annotations(paths: list[Path]) -> dict[str, dict[str, Any]]:
    result: dict[str, dict[str, Any]] = {}
    for path in paths:
        sources = sorted(path.glob("*.jsonl")) if path.is_dir() else [path]
        for source in sources:
            for annotation in load_jsonl(source):
                annotation_id = annotation["annotation_id"]
                if annotation_id in result:
                    raise ValueError(f"duplicate input annotation id: {annotation_id}")
                result[annotation_id] = annotation
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("adjudications", type=Path)
    parser.add_argument("--tasks", type=Path, default=Path("data/dedup-calibration.blind.jsonl"))
    parser.add_argument("--annotations", type=Path, nargs="+", required=True)
    parser.add_argument("--schema", type=Path, default=Path("docs/dedup-label-schema-v2.json"))
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    tasks = {task["item_id"]: task for task in load_jsonl(args.tasks)}
    annotations = load_annotations(args.annotations)
    adjudications = load_jsonl(args.adjudications)
    result = schema_validate(adjudications, args.schema)
    seen: set[str] = set()
    latest_by_item: dict[str, str] = {}
    for record in adjudications:
        adjudication_id = record["adjudication_id"]
        item_id = record["item_id"]
        if adjudication_id in seen:
            raise ValueError(f"duplicate adjudication id: {adjudication_id}")
        supersedes = record["supersedes_adjudication_id"]
        if item_id in latest_by_item:
            if supersedes != latest_by_item[item_id]:
                raise ValueError(f"{adjudication_id}: does not supersede the current item decision")
        elif supersedes is not None:
            raise ValueError(f"{adjudication_id}: supersedes an adjudication not yet in the ledger")
        seen.add(adjudication_id)
        latest_by_item[item_id] = adjudication_id
        if item_id not in tasks:
            raise ValueError(f"unknown adjudication item: {item_id}")
        inputs = [annotations.get(annotation_id) for annotation_id in record["input_annotation_ids"]]
        if any(annotation is None for annotation in inputs):
            raise ValueError(f"{adjudication_id}: missing input annotation")
        if any(annotation["item_id"] != item_id for annotation in inputs if annotation is not None):
            raise ValueError(f"{adjudication_id}: input annotations refer to another item")

        judgment = record["judgment"]
        factual = judgment["factual"]
        relations = factual["relations"]
        action = expected_policy(factual["entity_relation"], relations)
        if judgment["policy"]["action"] != action:
            raise ValueError(f"{adjudication_id}: inconsistent policy action")
        if record["status"] == "resolved" and factual["entity_relation"] == "insufficient_evidence":
            raise ValueError(f"{adjudication_id}: resolved adjudication cannot be insufficient")
        if record["status"] == "needs_more_evidence" and factual["entity_relation"] != "insufficient_evidence":
            raise ValueError(f"{adjudication_id}: evidence request must retain insufficient judgment")
        if record["status"] == "excluded":
            if factual["entity_relation"] != "insufficient_evidence":
                raise ValueError(f"{adjudication_id}: excluded item must not carry an identity verdict")
            if judgment["policy"]["action"] != "defer":
                raise ValueError(f"{adjudication_id}: excluded item must not produce a policy action")
        local_ids = evidence_ids(tasks[item_id], record["presented_order"])
        external_ids = {item["evidence_id"] for item in judgment["external_evidence"]}
        validate_primitive_relations(adjudication_id, tasks[item_id], relations, local_ids | external_ids)
        cited = {citation["ref"] for citation in judgment["citations"]}
        if cited - local_ids - external_ids:
            raise ValueError(f"{adjudication_id}: unknown judgment citation")
    effective_statuses = {
        record["item_id"]: record["status"] for record in adjudications
    }
    status_summary = ",".join(
        f"{status}={sum(value == status for value in effective_statuses.values())}"
        for status in ("resolved", "needs_more_evidence", "excluded")
    )
    print(
        f"adjudications={len(adjudications)} effective_items={len(latest_by_item)} "
        f"effective_statuses={status_summary} "
        f"inputs={len(annotations)} "
        f"invariants=passed schema={result}"
    )


if __name__ == "__main__":
    main()
