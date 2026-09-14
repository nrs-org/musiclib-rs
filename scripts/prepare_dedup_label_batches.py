#!/usr/bin/env python3
"""Prepare deterministic, side-randomized model-labeling bundles."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any


FORBIDDEN_KEYS = {
    "hidden",
    "sampling_frame",
    "legacy_softmatch",
    "current_entry_ids",
    "production_candidate",
    "proposers",
    "masked_links",
}


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    with path.open(encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, 1):
            if not line.strip():
                continue
            record = json.loads(line)
            if record.get("record_type") != "task":
                raise ValueError(f"{path}:{line_number}: expected a task")
            records.append(record)
    return records


def find_forbidden(value: Any, path: str = "$") -> list[str]:
    found: list[str] = []
    if isinstance(value, dict):
        for key, child in value.items():
            if key in FORBIDDEN_KEYS:
                found.append(f"{path}.{key}")
            found.extend(find_forbidden(child, f"{path}.{key}"))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.extend(find_forbidden(child, f"{path}[{index}]"))
    return found


def relabel_evidence(view: dict[str, Any], side: str) -> None:
    for record_number, record in enumerate(view["records"], 1):
        for fact in record["facts"]:
            parts = fact["evidence_id"].split(".", 2)
            if len(parts) != 3 or parts[0] not in {"L", "R"}:
                raise ValueError(f"unexpected evidence id: {fact['evidence_id']!r}")
            fact["evidence_id"] = f"{side}.{record_number}.{parts[2]}"


def present(task: dict[str, Any], swap: bool) -> dict[str, Any]:
    # JSON round-trip gives us a dependency-free deep copy.
    shown = json.loads(json.dumps(task, ensure_ascii=False))
    if swap:
        shown["left"], shown["right"] = shown["right"], shown["left"]
        order = ["right", "left"]
    else:
        order = ["left", "right"]
    relabel_evidence(shown["left"], "L")
    relabel_evidence(shown["right"], "R")
    return {"presented_order": order, "task": shown}


def base_swap(seed: str, item_id: str) -> bool:
    digest = hashlib.sha256(f"{seed}\0{item_id}".encode()).digest()
    return bool(digest[0] & 1)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=Path("data/dedup-calibration.blind.jsonl"))
    parser.add_argument("--output-dir", type=Path, default=Path("data/dedup-agent-batches/lane-a"))
    parser.add_argument("--lane", default="a", help="labeling lane recorded in the bundle")
    parser.add_argument("--seed", default="dedup-calibration-v1")
    parser.add_argument("--batch-size", type=int, default=5)
    parser.add_argument(
        "--max-chars",
        type=int,
        default=50000,
        help="approximate maximum compact-JSON characters per task bundle",
    )
    parser.add_argument(
        "--invert-sides",
        action="store_true",
        help="use the complementary presentation (recommended for a second vote)",
    )
    parser.add_argument(
        "--shuffle",
        action="store_true",
        help="deterministically mix task frames before packing batches",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.batch_size < 1:
        raise ValueError("--batch-size must be positive")
    if args.max_chars < 1000:
        raise ValueError("--max-chars must be at least 1000")
    tasks = load_jsonl(args.input)
    leaks = find_forbidden(tasks)
    if leaks:
        raise ValueError(f"input contains private selection metadata: {leaks[0]}")
    item_ids = [task["item_id"] for task in tasks]
    if len(item_ids) != len(set(item_ids)):
        raise ValueError("input contains duplicate item ids")
    if args.shuffle:
        tasks.sort(
            key=lambda task: hashlib.sha256(
                f"{args.seed}\0order\0{task['item_id']}".encode()
            ).digest()
        )

    args.output_dir.mkdir(parents=True, exist_ok=True)
    old_generated_files: set[str] = set()
    old_manifest_path = args.output_dir / "manifest.json"
    if old_manifest_path.exists():
        old_manifest = json.loads(old_manifest_path.read_text(encoding="utf-8"))
        if old_manifest.get("batch_version") in {"dedup-label-batch/1", "dedup-label-batch/2"}:
            old_generated_files = {
                item["file"]
                for item in old_manifest.get("batches", [])
                if isinstance(item, dict) and isinstance(item.get("file"), str)
            }
    manifest: dict[str, Any] = {
        "batch_version": "dedup-label-batch/2",
        "schema_version": "dedup-calibration/2",
        "prompt_version": "dedup-label-prompt/2",
        "lane": args.lane,
        "seed": args.seed,
        "invert_sides": args.invert_sides,
        "shuffled": args.shuffle,
        "source": str(args.input),
        "source_sha256": sha256_file(args.input),
        "task_count": len(tasks),
        "batch_size_limit": args.batch_size,
        "batch_character_limit": args.max_chars,
        "batches": [],
    }
    presented_tasks = [
        present(task, base_swap(args.seed, task["item_id"]) ^ args.invert_sides)
        for task in tasks
    ]
    batches: list[list[dict[str, Any]]] = []
    current: list[dict[str, Any]] = []
    current_chars = 0
    for item in presented_tasks:
        item_chars = len(json.dumps(item, ensure_ascii=False, separators=(",", ":")))
        if current and (len(current) >= args.batch_size or current_chars + item_chars > args.max_chars):
            batches.append(current)
            current = []
            current_chars = 0
        current.append(item)
        current_chars += item_chars
    if current:
        batches.append(current)

    for number, presented in enumerate(batches, 1):
        subset = [item["task"] for item in presented]
        batch_id = f"{args.lane}-{number:04d}"
        bundle = {
            "batch_version": "dedup-label-batch/2",
            "batch_id": batch_id,
            "lane": args.lane,
            "prompt_version": "dedup-label-prompt/2",
            "tasks": presented,
        }
        filename = f"batch-{number:04d}.json"
        with (args.output_dir / filename).open("w", encoding="utf-8") as handle:
            json.dump(bundle, handle, ensure_ascii=False, sort_keys=True, indent=2)
            handle.write("\n")
        manifest["batches"].append(
            {
                "batch_id": batch_id,
                "file": filename,
                "item_ids": [task["item_id"] for task in subset],
                "task_count": len(subset),
                "compact_task_chars": sum(
                    len(json.dumps(item, ensure_ascii=False, separators=(",", ":")))
                    for item in presented
                ),
            }
        )

    current_generated_files = {item["file"] for item in manifest["batches"]}
    for stale_name in old_generated_files - current_generated_files:
        stale_path = args.output_dir / stale_name
        if stale_path.parent == args.output_dir and stale_path.name.startswith("batch-"):
            stale_path.unlink(missing_ok=True)

    with (args.output_dir / "manifest.json").open("w", encoding="utf-8") as handle:
        json.dump(manifest, handle, ensure_ascii=False, sort_keys=True, indent=2)
        handle.write("\n")
    print(f"tasks={len(tasks)} batches={len(manifest['batches'])} lane={args.lane} output={args.output_dir}")


if __name__ == "__main__":
    main()
