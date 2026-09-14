#!/usr/bin/env python3
"""Validate dedup calibration private/blind JSONL ledgers.

This performs the cross-record invariants that JSON Schema cannot conveniently
express. If the optional ``jsonschema`` package is installed, records are also
validated against ``docs/dedup-label-schema-v2.json``.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
from pathlib import Path
from typing import Any


def canonical_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def sha256_value(value: Any) -> str:
    return "sha256:" + hashlib.sha256(canonical_json(value).encode("utf-8")).hexdigest()


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    with path.open(encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, 1):
            if not line.strip():
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError as error:
                raise ValueError(f"{path}:{line_number}: {error}") from error
    return records


def without_hidden(task: dict[str, Any]) -> dict[str, Any]:
    result = copy.deepcopy(task)
    result.pop("hidden", None)
    return result


def unprefix_evidence_id(value: str) -> str:
    parts = value.split(".", 2)
    if len(parts) == 3 and parts[0] in {"L", "R"} and parts[1].isdigit():
        return parts[2]
    raise ValueError(f"unprefixed public evidence id: {value!r}")


def generic_record(record: dict[str, Any]) -> dict[str, Any]:
    result = copy.deepcopy(record)
    claimed_hash = result.pop("evidence_sha256")
    for fact in result["facts"]:
        fact["evidence_id"] = unprefix_evidence_id(fact["evidence_id"])
    if sha256_value(result) != claimed_hash:
        raise ValueError(f"evidence hash mismatch for {record['record_id']}")
    return {"evidence_sha256": claimed_hash, **result}


def generic_view(view: dict[str, Any]) -> dict[str, Any]:
    result = copy.deepcopy(view)
    claimed_hash = result.pop("view_id")
    result["records"] = [generic_record(record) for record in result["records"]]
    if sha256_value(result) != claimed_hash:
        raise ValueError(f"view hash mismatch for {claimed_hash}")
    return {"view_id": claimed_hash, **result}


def validate_task(task: dict[str, Any]) -> None:
    if task.get("record_type") != "task":
        raise ValueError("calibration task ledger contains a non-task record")
    left = generic_view(task["left"])
    right = generic_view(task["right"])
    if left["view_id"] == right["view_id"]:
        raise ValueError(f"{task['item_id']}: identical view ids")
    if task["entry_type"] != left["entry_type"] or task["entry_type"] != right["entry_type"]:
        raise ValueError(f"{task['item_id']}: entry type mismatch")
    left_records = {record["record_id"] for record in left["records"]}
    right_records = {record["record_id"] for record in right["records"]}
    if left_records & right_records:
        raise ValueError(f"{task['item_id']}: overlapping source records")

    expected_item_id = sha256_value(
        [
            task["schema_version"],
            task["snapshot_id"],
            task["ontology_version"],
            task["policy_version"],
            task["entry_type"],
            *sorted([left["view_id"], right["view_id"]]),
        ]
    )
    if expected_item_id != task["item_id"]:
        raise ValueError(f"{task['item_id']}: item hash mismatch")

    evidence_ids = [
        fact["evidence_id"]
        for view in [task["left"], task["right"]]
        for record in view["records"]
        for fact in record["facts"]
    ]
    if len(evidence_ids) != len(set(evidence_ids)):
        raise ValueError(f"{task['item_id']}: duplicate evidence ids")


def schema_validate(records: list[dict[str, Any]], schema_path: Path) -> str:
    try:
        import jsonschema
    except ImportError:
        return "skipped (install python3Packages.jsonschema for JSON Schema validation)"
    with schema_path.open(encoding="utf-8") as handle:
        schema = json.load(handle)
    validator = jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker())
    for index, record in enumerate(records, 1):
        errors = sorted(validator.iter_errors(record), key=lambda error: list(error.path))
        if errors:
            error = errors[0]
            location = ".".join(str(part) for part in error.path)
            raise ValueError(f"record {index} schema error at {location or '<root>'}: {error.message}")
    return "passed"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--private", type=Path, default=Path("data/dedup-calibration.private.jsonl"))
    parser.add_argument("--blind", type=Path, default=Path("data/dedup-calibration.blind.jsonl"))
    parser.add_argument(
        "--schema", type=Path, default=Path("docs/dedup-label-schema-v2.json")
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    private = load_jsonl(args.private)
    blind = load_jsonl(args.blind)
    if len(private) != len(blind):
        raise ValueError(f"private/blind record count differs: {len(private)} != {len(blind)}")

    seen: set[str] = set()
    for index, (private_task, blind_task) in enumerate(zip(private, blind, strict=True), 1):
        if "hidden" not in private_task:
            raise ValueError(f"private record {index} has no hidden selection metadata")
        if "hidden" in blind_task:
            raise ValueError(f"blind record {index} leaks hidden selection metadata")
        if without_hidden(private_task) != blind_task:
            raise ValueError(f"private/blind projection differs at record {index}")
        if private_task["item_id"] in seen:
            raise ValueError(f"duplicate item id: {private_task['item_id']}")
        seen.add(private_task["item_id"])
        validate_task(private_task)

    result = schema_validate(private + blind, args.schema)
    print(f"records={len(private)} private_blind_projection=passed invariants=passed schema={result}")


if __name__ == "__main__":
    main()
