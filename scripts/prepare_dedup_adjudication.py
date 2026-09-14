#!/usr/bin/env python3
"""Build a blind queue for items whose two annotation passes disagree."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

from prepare_dedup_label_batches import present
from validate_dedup_ledger import load_jsonl


def annotation_map(path: Path) -> dict[str, dict[str, Any]]:
    paths = sorted(path.glob("*.jsonl")) if path.is_dir() else [path]
    result: dict[str, dict[str, Any]] = {}
    for item_path in paths:
        for annotation in load_jsonl(item_path):
            item_id = annotation["item_id"]
            if item_id in result:
                raise ValueError(f"duplicate annotation for {item_id} under {path}")
            result[item_id] = annotation
    return result


def factual_label(annotation: dict[str, Any]) -> str:
    return str(annotation["judgment"]["factual"]["entity_relation"])


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tasks", type=Path, default=Path("data/dedup-calibration.blind.jsonl"))
    parser.add_argument("--pass-a", type=Path, default=Path("data/dedup-agent-labels/luna-low-a"))
    parser.add_argument("--pass-b", type=Path, default=Path("data/dedup-agent-labels/luna-low-b"))
    parser.add_argument("--output-dir", type=Path, default=Path("data/dedup-adjudication/blind"))
    parser.add_argument(
        "--private-output",
        type=Path,
        default=Path("data/dedup-adjudication/disagreements.private.jsonl"),
    )
    parser.add_argument("--seed", default="dedup-adjudication-v2")
    parser.add_argument("--batch-size", type=int, default=4)
    parser.add_argument("--max-chars", type=int, default=50000)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    tasks = {task["item_id"]: task for task in load_jsonl(args.tasks)}
    pass_a = annotation_map(args.pass_a)
    pass_b = annotation_map(args.pass_b)
    if set(pass_a) != set(tasks) or set(pass_b) != set(tasks):
        raise ValueError("both annotation passes must cover the complete task ledger")

    disagreement_ids = [
        item_id
        for item_id in tasks
        if factual_label(pass_a[item_id]) != factual_label(pass_b[item_id])
    ]
    disagreement_ids.sort(
        key=lambda item_id: hashlib.sha256(f"{args.seed}\0order\0{item_id}".encode()).digest()
    )
    presented = [
        present(
            tasks[item_id],
            bool(hashlib.sha256(f"{args.seed}\0side\0{item_id}".encode()).digest()[0] & 1),
        )
        for item_id in disagreement_ids
    ]

    batches: list[list[dict[str, Any]]] = []
    current: list[dict[str, Any]] = []
    current_chars = 0
    for item in presented:
        item_chars = len(json.dumps(item, ensure_ascii=False, separators=(",", ":")))
        if current and (len(current) >= args.batch_size or current_chars + item_chars > args.max_chars):
            batches.append(current)
            current, current_chars = [], 0
        current.append(item)
        current_chars += item_chars
    if current:
        batches.append(current)

    args.output_dir.mkdir(parents=True, exist_ok=True)
    args.private_output.parent.mkdir(parents=True, exist_ok=True)
    manifest = {
        "batch_version": "dedup-adjudication-blind/1",
        "schema_version": "dedup-calibration/2",
        "prompt_version": "dedup-label-prompt/2",
        "task_count": len(disagreement_ids),
        "batches": [],
    }
    for number, batch in enumerate(batches, 1):
        filename = f"batch-{number:04d}.json"
        bundle = {
            "batch_version": "dedup-adjudication-blind/1",
            "batch_id": f"adjudication-{number:04d}",
            "prompt_version": "dedup-label-prompt/2",
            "tasks": batch,
        }
        with (args.output_dir / filename).open("w", encoding="utf-8") as handle:
            json.dump(bundle, handle, ensure_ascii=False, sort_keys=True, indent=2)
            handle.write("\n")
        manifest["batches"].append(
            {
                "batch_id": bundle["batch_id"],
                "file": filename,
                "item_ids": [item["task"]["item_id"] for item in batch],
                "task_count": len(batch),
            }
        )
    with (args.output_dir / "manifest.json").open("w", encoding="utf-8") as handle:
        json.dump(manifest, handle, ensure_ascii=False, sort_keys=True, indent=2)
        handle.write("\n")

    with args.private_output.open("w", encoding="utf-8") as handle:
        for item_id in disagreement_ids:
            record = {
                "item_id": item_id,
                "annotation_a": pass_a[item_id],
                "annotation_b": pass_b[item_id],
            }
            handle.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")
    print(f"disagreements={len(disagreement_ids)} batches={len(batches)} output={args.output_dir}")


if __name__ == "__main__":
    main()
