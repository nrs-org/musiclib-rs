#!/usr/bin/env python3
"""Append provenance-bearing supersessions from targeted evidence decisions."""

from __future__ import annotations

import argparse
import copy
import datetime as dt
import hashlib
import json
from pathlib import Path
from typing import Any

from validate_dedup_ledger import canonical_json, load_jsonl, sha256_value


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tasks", type=Path, default=Path("data/dedup-calibration.blind.jsonl"))
    parser.add_argument("--input", type=Path, default=Path("data/dedup-adjudications.jsonl"))
    parser.add_argument(
        "--decisions",
        type=Path,
        default=Path("data/dedup-adjudication/musicbrainz-enrichment.jsonl"),
    )
    parser.add_argument(
        "--output", type=Path, default=Path("data/dedup-adjudications.enriched.jsonl")
    )
    parser.add_argument("--snapshot", default="musicbrainz-replication:188093")
    parser.add_argument("--run-id", default="musicbrainz-enrichment-188093")
    parser.add_argument("--evidence-prefix", default="MB")
    parser.add_argument("--disagreement-tag", default="targeted_musicbrainz_evidence")
    parser.add_argument(
        "--resolution-reason",
        default="Superseded after a targeted query against the pinned local MusicBrainz snapshot.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    tasks = {task["item_id"]: task for task in load_jsonl(args.tasks)}
    originals = load_jsonl(args.input)
    by_item = {record["item_id"]: record for record in originals}
    decisions = load_jsonl(args.decisions)
    now = dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z")
    supersessions: list[dict[str, Any]] = []
    for number, decision in enumerate(decisions, 1):
        matches = [item_id for item_id in tasks if item_id.removeprefix("sha256:").startswith(decision["item_id_prefix"])]
        if len(matches) != 1:
            raise ValueError(f"prefix {decision['item_id_prefix']} resolved to {len(matches)} items")
        item_id = matches[0]
        old = by_item[item_id]
        if old["status"] != "needs_more_evidence":
            raise ValueError(f"{item_id}: enrichment does not supersede an evidence request")
        judgment = copy.deepcopy(old["judgment"])
        entity_relation = decision["entity_relation"]
        evidence_specs = decision.get("evidence") or [decision]
        evidence_ids: list[str] = []
        externals: list[dict[str, Any]] = []
        for evidence_number, evidence_spec in enumerate(evidence_specs, 1):
            suffix = str(number) if len(evidence_specs) == 1 else f"{number}.{evidence_number}"
            evidence_id = f"{args.evidence_prefix}.{suffix}"
            evidence_ids.append(evidence_id)
            evidence_claim = evidence_spec.get("claim", decision["claim"])
            snapshot = evidence_spec.get("snapshot", decision.get("snapshot", args.snapshot))
            externals.append(
                {
                    "evidence_id": evidence_id,
                    "url": evidence_spec["url"],
                    "accessed_at": now,
                    "snapshot_sha256": sha256_value([snapshot, evidence_claim]),
                    "claim": evidence_claim,
                }
            )
        judgment["input_integrity"] = "ok"
        judgment["factual"]["entity_relation"] = entity_relation
        judgment["factual"]["relations"] = []
        judgment["external_evidence"].extend(externals)
        if entity_relation == "same_identity":
            judgment["policy"] = {"action": "merge", "exception_reason": None}
            citation_role = "supports_same_identity"
        else:
            relation_spec = decision.get("relation")
            if relation_spec:
                task = tasks[item_id]
                relation = {
                    "predicate": relation_spec["predicate"],
                    "subjects": [
                        {
                            "ref_kind": "view",
                            "entity_type": task["entry_type"],
                            "ref": task[side]["view_id"],
                        }
                        for side in ("left", "right")
                    ],
                    "object": {
                        "ref_kind": "provider",
                        "entity_type": relation_spec["object_entity_type"],
                        "ref": relation_spec["object_ref"],
                    },
                    "metadata": relation_spec["metadata"],
                    "evidence_refs": evidence_ids,
                }
                judgment["factual"]["relations"] = [relation]
                judgment["policy"] = {"action": "relate", "exception_reason": None}
                citation_role = "supports_primitive_relation"
            else:
                judgment["policy"] = {"action": "keep_separate", "exception_reason": None}
                citation_role = "supports_different_identity"
        judgment["citations"].extend(
            {"ref": evidence_id, "role": citation_role} for evidence_id in evidence_ids
        )
        judgment["confidence"] = {
            "label_probability": decision["confidence"],
            "evidence_quality": "high",
        }
        judgment["rationale"] = decision["claim"]
        input_ids = old["input_annotation_ids"]
        digest = hashlib.sha256(
            canonical_json(["dedup-adjudication/2", item_id, old["adjudication_id"], judgment]).encode()
        ).hexdigest()
        supersessions.append(
            {
                "record_type": "adjudication",
                "schema_version": "dedup-calibration/2",
                "adjudication_id": f"sha256:{digest}",
                "item_id": item_id,
                "created_at": now,
                "input_annotation_ids": input_ids,
                "presented_order": old["presented_order"],
                "status": "resolved",
                "adjudicator": {
                    "kind": "model",
                    "annotator_id": "root-reconciler",
                    "provider": "openai",
                    "model": "unknown",
                    "guideline_version": "dedup-reconciliation/2",
                    "run_id": args.run_id,
                },
                "disagreement_tags": [args.disagreement_tag],
                "judgment": judgment,
                "resolution_reason": args.resolution_reason,
                "supersedes_adjudication_id": old["adjudication_id"],
            }
        )

    with args.output.open("w", encoding="utf-8") as handle:
        for record in [*originals, *supersessions]:
            handle.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")
    print(f"original={len(originals)} supersessions={len(supersessions)} output={args.output}")


if __name__ == "__main__":
    main()
