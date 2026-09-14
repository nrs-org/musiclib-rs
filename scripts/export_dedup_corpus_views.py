#!/usr/bin/env python3
"""Export complete entry views from a read-only musiclib snapshot as JSONL."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from export_dedup_calibration import Snapshot, canonical_json, sha256_file


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--max-records-per-view", type=int, default=20)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if not args.db.exists():
        raise SystemExit(f"musiclib database not found: {args.db}")
    snapshot_id = sha256_file(args.db)
    snapshot = Snapshot(args.db, args.max_records_per_view)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    try:
        with args.output.open("w", encoding="utf-8") as handle:
            for entry_id in sorted(snapshot.entry_types):
                view = snapshot.view(entry_id)
                handle.write(
                    canonical_json(
                        {
                            "entry_id": entry_id,
                            "snapshot_id": snapshot_id,
                            "view": view,
                        }
                    )
                    + "\n"
                )
                count += 1
    finally:
        snapshot.close()
    print(f"entries={count} snapshot={snapshot_id} output={args.output}")


if __name__ == "__main__":
    main()
