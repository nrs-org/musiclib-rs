#!/usr/bin/env python3
"""Validate dedup annotations against blind tasks and ontology invariants."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from validate_dedup_ledger import load_jsonl, schema_validate


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "annotations",
        type=Path,
        nargs="+",
        help="annotation JSONL file(s), or directories containing *.jsonl",
    )
    parser.add_argument("--tasks", type=Path, default=Path("data/dedup-calibration.blind.jsonl"))
    parser.add_argument("--schema", type=Path, default=Path("docs/dedup-label-schema-v2.json"))
    parser.add_argument(
        "--batches",
        type=Path,
        help="optional batch directory used to verify each presented_order",
    )
    parser.add_argument(
        "--require-complete",
        action="store_true",
        help="require exactly one annotation for every task",
    )
    return parser.parse_args()


def evidence_ids(task: dict[str, Any], presented_order: list[str]) -> set[str]:
    result: set[str] = set()
    for displayed_side, canonical_side in zip(("L", "R"), presented_order, strict=True):
        for record_number, record in enumerate(task[canonical_side]["records"], 1):
            for fact in record["facts"]:
                parts = fact["evidence_id"].split(".", 2)
                if len(parts) != 3:
                    raise ValueError(f"unexpected task evidence id: {fact['evidence_id']!r}")
                result.add(f"{displayed_side}.{record_number}.{parts[2]}")
    return result


def expected_policy(entity_relation: str, relations: list[dict[str, Any]]) -> str:
    if entity_relation == "same_identity":
        return "merge"
    if entity_relation == "insufficient_evidence":
        return "defer"
    if entity_relation == "different_identity":
        return "relate" if relations else "keep_separate"
    raise ValueError(f"unknown entity relation: {entity_relation}")


def validate_primitive_relations(
    annotation_id: str,
    task: dict[str, Any],
    relations: list[dict[str, Any]],
    allowed_evidence: set[str],
) -> None:
    task_views = {task["left"]["view_id"], task["right"]["view_id"]}
    for index, relation in enumerate(relations, 1):
        predicate = relation["predicate"]
        if predicate in {"derived_from", "facet_of"} and len(relation["subjects"]) != 1:
            raise ValueError(f"{annotation_id}: relation {index} {predicate} requires one subject")

        refs = [*relation["subjects"], relation["object"]]
        view_refs: set[str] = set()
        for ref in refs:
            if ref["ref_kind"] == "view":
                if ref["ref"] not in task_views:
                    raise ValueError(
                        f"{annotation_id}: relation {index} refers to a view outside the task"
                    )
                if ref["entity_type"] != task["entry_type"]:
                    raise ValueError(
                        f"{annotation_id}: relation {index} view has the wrong entity type"
                    )
                view_refs.add(ref["ref"])
            elif ref["ref_kind"] == "hypothesis" and not ref["ref"].startswith("hypothesis:"):
                raise ValueError(
                    f"{annotation_id}: relation {index} hypothesis ref must start with 'hypothesis:'"
                )
        if view_refs != task_views:
            raise ValueError(
                f"{annotation_id}: relation {index} must account for both compared task views"
            )
        subject_keys = {(ref["ref_kind"], ref["ref"]) for ref in relation["subjects"]}
        object_key = (relation["object"]["ref_kind"], relation["object"]["ref"])
        if object_key in subject_keys:
            raise ValueError(f"{annotation_id}: relation {index} has the same subject and object")

        unknown = set(relation["evidence_refs"]) - allowed_evidence
        if unknown:
            raise ValueError(
                f"{annotation_id}: relation {index} has unknown evidence refs: {sorted(unknown)}"
            )


def main() -> None:
    args = parse_args()
    tasks = {task["item_id"]: task for task in load_jsonl(args.tasks)}
    presented_orders: dict[str, list[str]] = {}
    if args.batches is not None:
        for batch_path in sorted(args.batches.glob("batch-*.json")):
            with batch_path.open(encoding="utf-8") as handle:
                bundle = json.load(handle)
            for item in bundle["tasks"]:
                item_id = item["task"]["item_id"]
                if item_id in presented_orders:
                    raise ValueError(f"duplicate task across batch files: {item_id}")
                presented_orders[item_id] = item["presented_order"]
    annotation_paths: list[Path] = []
    for path in args.annotations:
        if path.is_dir():
            annotation_paths.extend(sorted(path.glob("*.jsonl")))
        else:
            annotation_paths.append(path)
    if not annotation_paths:
        raise ValueError("no annotation JSONL files found")
    annotations = [
        annotation
        for path in annotation_paths
        for annotation in load_jsonl(path)
    ]
    result = schema_validate(annotations, args.schema)
    seen_ids: set[str] = set()
    seen_votes: set[tuple[str, str]] = set()

    for annotation in annotations:
        if annotation.get("record_type") != "annotation":
            raise ValueError("annotation ledger contains a non-annotation record")
        item_id = annotation["item_id"]
        if item_id not in tasks:
            raise ValueError(f"annotation refers to unknown item: {item_id}")
        if presented_orders and annotation["presented_order"] != presented_orders.get(item_id):
            raise ValueError(f"{annotation['annotation_id']}: presented_order differs from batch")
        annotation_id = annotation["annotation_id"]
        if annotation_id in seen_ids:
            raise ValueError(f"duplicate annotation id: {annotation_id}")
        seen_ids.add(annotation_id)
        annotator_id = annotation["annotator"]["annotator_id"]
        vote = (item_id, annotator_id)
        if vote in seen_votes:
            raise ValueError(f"duplicate vote for item {item_id} by {annotator_id}")
        seen_votes.add(vote)

        judgment = annotation["judgment"]
        relation = judgment["factual"]["entity_relation"]
        relations = judgment["factual"]["relations"]
        action = judgment["policy"]["action"]
        expected_action = expected_policy(relation, relations)
        if action != expected_action:
            raise ValueError(f"{annotation_id}: factual judgment requires policy {expected_action}")
        if relation in {"same_identity", "insufficient_evidence"} and relations:
            raise ValueError(f"{annotation_id}: {relation} requires an empty relations list")

        local_ids = evidence_ids(tasks[item_id], annotation["presented_order"])
        external_ids = {item["evidence_id"] for item in judgment["external_evidence"]}
        validate_primitive_relations(annotation_id, tasks[item_id], relations, local_ids | external_ids)
        unknown = {citation["ref"] for citation in judgment["citations"]} - local_ids - external_ids
        if unknown:
            raise ValueError(f"{annotation_id}: unknown citation(s): {sorted(unknown)}")

    if args.require_complete:
        annotated_items = {annotation["item_id"] for annotation in annotations}
        missing = set(tasks) - annotated_items
        extra = annotated_items - set(tasks)
        if missing or extra or len(annotations) != len(tasks):
            raise ValueError(
                "incomplete task coverage: "
                f"annotations={len(annotations)} unique_items={len(annotated_items)} "
                f"missing={len(missing)} extra={len(extra)}"
            )
        if presented_orders and set(presented_orders) != set(tasks):
            raise ValueError(
                f"batch coverage differs from task ledger: batches={len(presented_orders)} "
                f"tasks={len(tasks)}"
            )

    print(
        f"files={len(annotation_paths)} annotations={len(annotations)} tasks={len(tasks)} "
        f"invariants=passed schema={result}"
    )


if __name__ == "__main__":
    main()
