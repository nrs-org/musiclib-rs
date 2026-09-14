#!/usr/bin/env python3
"""Append authoritative human decisions to the dedup adjudication ledger."""

from __future__ import annotations

import argparse
import copy
import datetime as dt
import hashlib
import json
from pathlib import Path
from typing import Any

from validate_dedup_ledger import canonical_json, load_jsonl


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tasks", type=Path, default=Path("data/dedup-calibration.blind.jsonl"))
    parser.add_argument("--input", type=Path, default=Path("data/dedup-adjudications.final.jsonl"))
    parser.add_argument(
        "--decisions",
        type=Path,
        default=Path("data/dedup-adjudication/human-decisions.jsonl"),
    )
    parser.add_argument(
        "--output", type=Path, default=Path("data/dedup-adjudications.final.jsonl")
    )
    parser.add_argument("--annotator-id", default="project-owner")
    return parser.parse_args()


def displayed_evidence_id(task: dict[str, Any], presented_order: list[str], canonical_side: str) -> str:
    displayed_side = "L" if presented_order[0] == canonical_side else "R"
    view = task[canonical_side]
    for record_number, record in enumerate(view["records"], 1):
        for fact in record["facts"]:
            if fact["field"] == "alias":
                suffix = fact["evidence_id"].split(".", 2)[2]
                return f"{displayed_side}.{record_number}.{suffix}"
    for record_number, record in enumerate(view["records"], 1):
        if record["facts"]:
            suffix = record["facts"][0]["evidence_id"].split(".", 2)[2]
            return f"{displayed_side}.{record_number}.{suffix}"
    raise ValueError(f"{task['item_id']}: view {canonical_side} has no citable facts")


def main() -> None:
    args = parse_args()
    tasks = {task["item_id"]: task for task in load_jsonl(args.tasks)}
    records = load_jsonl(args.input)
    current = {record["item_id"]: record for record in records}
    now = dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z")
    appended: list[dict[str, Any]] = []

    for decision in load_jsonl(args.decisions):
        matches = [
            item_id
            for item_id in tasks
            if item_id.removeprefix("sha256:").startswith(decision["item_id_prefix"])
        ]
        if len(matches) != 1:
            raise ValueError(
                f"prefix {decision['item_id_prefix']} resolved to {len(matches)} items"
            )
        item_id = matches[0]
        old = current[item_id]
        if old["status"] != "needs_more_evidence":
            raise ValueError(f"{item_id}: human decision must supersede an evidence request")

        task = tasks[item_id]
        judgment = copy.deepcopy(old["judgment"])
        kind = decision["decision"]
        relations: list[dict[str, Any]] = []
        if kind == "same":
            entity_relation = "same_identity"
            action = "merge"
            status = "resolved"
        elif kind == "different":
            entity_relation = "different_identity"
            action = "keep_separate"
            status = "resolved"
        elif kind == "different_edition":
            entity_relation = "different_identity"
            action = "relate"
            status = "resolved"
            evidence_refs = [
                displayed_evidence_id(task, old["presented_order"], side)
                for side in ("left", "right")
            ]
            relations = [
                {
                    "predicate": "member_of",
                    "subjects": [
                        {
                            "ref_kind": "view",
                            "entity_type": task["entry_type"],
                            "ref": task[side]["view_id"],
                        }
                        for side in ("left", "right")
                    ],
                    "object": {
                        "ref_kind": "hypothesis",
                        "entity_type": "recording_family",
                        "ref": f"hypothesis:recording-family:{decision['family']}",
                        "label": decision["family"],
                    },
                    "metadata": {"variant": "different_edition"},
                    "evidence_refs": evidence_refs,
                }
            ]
            judgment["citations"].extend(
                {"ref": ref, "role": "supports_primitive_relation"}
                for ref in evidence_refs
            )
        elif kind == "exclude":
            entity_relation = "insufficient_evidence"
            action = "defer"
            status = "excluded"
        else:
            raise ValueError(f"{item_id}: unsupported human decision {kind!r}")

        judgment["factual"] = {
            "entity_relation": entity_relation,
            "relations": relations,
        }
        judgment["policy"] = {"action": action, "exception_reason": None}
        judgment["confidence"] = {"label_probability": 1.0, "evidence_quality": "high"}
        judgment["rationale"] = f"Project-owner adjudication: {decision['note']}"

        digest = hashlib.sha256(
            canonical_json(
                ["dedup-adjudication/human/1", item_id, old["adjudication_id"], judgment, status]
            ).encode()
        ).hexdigest()
        record = {
            "record_type": "adjudication",
            "schema_version": "dedup-calibration/2",
            "adjudication_id": f"sha256:{digest}",
            "item_id": item_id,
            "created_at": now,
            "input_annotation_ids": old["input_annotation_ids"],
            "presented_order": old["presented_order"],
            "status": status,
            "adjudicator": {
                "kind": "human",
                "annotator_id": args.annotator_id,
                "guideline_version": "dedup-reconciliation/3",
                "interface_version": "conversation",
                "run_id": "project-owner-adjudication-2026-09-12",
            },
            "disagreement_tags": ["authoritative_human_decision"],
            "judgment": judgment,
            "resolution_reason": decision["note"],
            "supersedes_adjudication_id": old["adjudication_id"],
        }
        appended.append(record)
        current[item_id] = record

    with args.output.open("w", encoding="utf-8") as handle:
        for record in [*records, *appended]:
            handle.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")
    print(f"original={len(records)} human_supersessions={len(appended)} output={args.output}")


if __name__ == "__main__":
    main()
