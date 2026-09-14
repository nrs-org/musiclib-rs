#!/usr/bin/env python3
"""Reconcile two disagreeing votes with an independent blind third judgment."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
from pathlib import Path
from typing import Any

from validate_dedup_ledger import canonical_json, load_jsonl


def load_map(path: Path) -> dict[str, dict[str, Any]]:
    paths = sorted(path.glob("*.jsonl")) if path.is_dir() else [path]
    result: dict[str, dict[str, Any]] = {}
    for item_path in paths:
        for annotation in load_jsonl(item_path):
            item_id = annotation["item_id"]
            if item_id in result:
                raise ValueError(f"duplicate annotation for {item_id} under {path}")
            result[item_id] = annotation
    return result


def label(annotation: dict[str, Any]) -> str:
    return str(annotation["judgment"]["factual"]["entity_relation"])


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pass-a", type=Path, default=Path("data/dedup-agent-labels/luna-low-a"))
    parser.add_argument("--pass-b", type=Path, default=Path("data/dedup-agent-labels/luna-low-b"))
    parser.add_argument(
        "--third-pass", type=Path, default=Path("data/dedup-adjudication/third-pass-astra")
    )
    parser.add_argument("--output", type=Path, default=Path("data/dedup-adjudications.jsonl"))
    parser.add_argument(
        "--effective-output",
        type=Path,
        default=None,
        help="optional complete annotation ledger: pass A for agreements, blind third vote for disagreements",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    pass_a, pass_b, third = load_map(args.pass_a), load_map(args.pass_b), load_map(args.third_pass)
    expected = {item_id for item_id in pass_a if label(pass_a[item_id]) != label(pass_b[item_id])}
    if set(third) != expected:
        raise ValueError(
            f"third-pass coverage mismatch: expected={len(expected)} actual={len(third)}"
        )

    now = dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z")
    records: list[dict[str, Any]] = []
    for item_id in sorted(expected):
        a, b, c = pass_a[item_id], pass_b[item_id], third[item_id]
        la, lb, lc = label(a), label(b), label(c)
        input_ids = [a["annotation_id"], b["annotation_id"], c["annotation_id"]]
        tags = ["direct_conflict" if "insufficient_evidence" not in {la, lb} else "abstention_disagreement"]
        if lc == "insufficient_evidence":
            status = "needs_more_evidence"
            tags.append("blind_adjudicator_abstained")
            reason = (
                "Independent blind adjudication found the packet insufficient; "
                "retain all votes and request targeted evidence enrichment."
            )
        else:
            status = "resolved"
            if lc == la:
                tags.append("blind_adjudicator_confirmed_a")
            if lc == lb:
                tags.append("blind_adjudicator_confirmed_b")
            reason = (
                "Independent blind adjudication reached a decisive evidence-cited judgment "
                "that agrees with one original vote."
            )
        digest = hashlib.sha256(
            canonical_json(["dedup-adjudication/1", item_id, input_ids, c["judgment"]]).encode()
        ).hexdigest()
        records.append(
            {
                "record_type": "adjudication",
                "schema_version": "dedup-calibration/2",
                "adjudication_id": f"sha256:{digest}",
                "item_id": item_id,
                "created_at": now,
                "input_annotation_ids": input_ids,
                "presented_order": c["presented_order"],
                "status": status,
                "adjudicator": {
                    "kind": "model",
                    "annotator_id": "root-reconciler",
                    "provider": "openai",
                    "model": "unknown",
                    "guideline_version": "dedup-reconciliation/1",
                    "run_id": "blind-adjudication-v2-1",
                },
                "disagreement_tags": tags,
                "judgment": c["judgment"],
                "resolution_reason": reason,
                "supersedes_adjudication_id": None,
            }
        )

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")
    if args.effective_output is not None:
        effective = {
            item_id: (pass_a[item_id] if label(pass_a[item_id]) == label(pass_b[item_id]) else third[item_id])
            for item_id in pass_a
        }
        args.effective_output.parent.mkdir(parents=True, exist_ok=True)
        with args.effective_output.open("w", encoding="utf-8") as handle:
            for item_id in sorted(effective):
                handle.write(json.dumps(effective[item_id], ensure_ascii=False, sort_keys=True) + "\n")
    counts: dict[str, int] = {}
    for record in records:
        counts[record["status"]] = counts.get(record["status"], 0) + 1
    print(
        f"adjudications={len(records)} statuses={json.dumps(counts, sort_keys=True)} "
        f"output={args.output} effective_output={args.effective_output}"
    )


if __name__ == "__main__":
    main()
