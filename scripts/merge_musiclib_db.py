#!/usr/bin/env python3
"""Merge two musiclib SQLite DBs into a new file.

The base DB is copied to OUT and the other DB is folded into it. Neither input
is modified. Identity is the `(source, identifier)` pair, so the merge mirrors
what the importer would do had both libraries been imported into one DB:

* Entries are unioned across the two DBs whenever they share a pair (and
  transitively). The surviving id is the smallest base id in the component;
  entries only in the other DB are renumbered by a fixed offset.
* `entry_source` rows for shared pairs keep the fetched row over a stub, and the
  newer fetch over an older one (`upsert_pair` / `upsert_stub_pair` semantics).
* Aliases and contributions are inserted only when no identical row exists
  (`insert_aliases_for_pair` / `insert_contribution`); child edges are
  idempotent on their PK, preferring the side whose parent row won.
* `entry_relation` rows are remapped like `merge_entries`: endpoints
  normalized, self-relations dropped, duplicates combined (higher confidence
  wins, `extra` payloads unioned).
* `dedup_feedback` is an append-only ledger: rows are remapped unless that
  would make them a self-pair, in which case they are left untouched (as
  `merge_entries` does) and reported.
* `dedup_block_key` rows of merged entries are dropped so `dedup` reindexes
  them.

Afterwards run `cargo run --bin dedup -- --db OUT` to re-apply the dedup
barrier across the merged library.
"""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys
from collections import Counter
from pathlib import Path


class Dsu:
    def __init__(self) -> None:
        self.parent: dict[tuple[str, int], tuple[str, int]] = {}

    def find(self, x: tuple[str, int]) -> tuple[str, int]:
        self.parent.setdefault(x, x)
        root = x
        while self.parent[root] != root:
            root = self.parent[root]
        while self.parent[x] != root:
            self.parent[x], x = root, self.parent[x]
        return root

    def union(self, a: tuple[str, int], b: tuple[str, int]) -> None:
        ra, rb = self.find(a), self.find(b)
        if ra != rb:
            self.parent[rb] = ra


def combine_extra(a: str | None, b: str | None) -> str | None:
    """Port of `combine_extra` in src/musicdb/mod.rs."""
    if a is None or b is None:
        return a if b is None else b
    if a == b:
        return a
    items = []
    for raw in (a, b):
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            value = raw
        items.extend(value if isinstance(value, list) else [value])
    keyed = {json.dumps(v, separators=(",", ":"), ensure_ascii=False): v for v in items}
    return json.dumps([keyed[k] for k in sorted(keyed)], separators=(",", ":"), ensure_ascii=False)


def combine_relations(a: dict, b: dict) -> dict:
    """Port of `combine_relations` in src/musicdb/mod.rs."""
    if a["confidence"] != b["confidence"]:
        a_wins = a["confidence"] > b["confidence"]
    else:
        a_wins = (a["origin"], a["enabled"]) >= (b["origin"], b["enabled"])
    primary = dict(a if a_wins else b)
    primary["extra"] = combine_extra(a["extra"], b["extra"])
    return primary


def source_row_wins(other: sqlite3.Row, base: sqlite3.Row) -> bool:
    """Whether the other DB's `entry_source` row should replace the base row."""
    if other["fetched_at"] is None:
        return False
    return base["fetched_at"] is None or other["fetched_at"] > base["fetched_at"]


def copy_db(src: Path, dst: Path) -> None:
    source = sqlite3.connect(f"file:{src}?mode=ro", uri=True)
    target = sqlite3.connect(dst)
    source.backup(target)
    source.close()
    target.close()


def merge(base_path: Path, other_path: Path, out_path: Path) -> None:
    if out_path.exists():
        sys.exit(f"refusing to overwrite existing {out_path}")
    copy_db(base_path, out_path)

    db = sqlite3.connect(f"file:{out_path}", uri=True, isolation_level=None)
    db.row_factory = sqlite3.Row
    db.execute("ATTACH DATABASE ? AS o", (f"file:{other_path}?mode=ro",))
    db.execute("BEGIN")
    stats: Counter[str] = Counter()

    for name in ("main", "o"):
        pending = db.execute(
            f"SELECT count(*) FROM {name}.import_run WHERE status NOT IN ('committed', 'rolled_back')"
        ).fetchone()[0]
        if pending:
            sys.exit(f"{name} has {pending} uncommitted import run(s); recover them first")

    # --- Entry identity: union entries that share a pair. ---------------------
    base_types = dict(db.execute("SELECT id, entry_type FROM main.entry"))
    other_types = dict(db.execute("SELECT id, entry_type FROM o.entry"))
    dsu = Dsu()
    for entry_id in base_types:
        dsu.find(("b", entry_id))
    for entry_id in other_types:
        dsu.find(("o", entry_id))
    shared = db.execute(
        "SELECT b.entry_id AS b, x.entry_id AS o FROM main.entry_source b "
        "JOIN o.entry_source x USING (source, identifier)"
    ).fetchall()
    for row in shared:
        dsu.union(("b", row["b"]), ("o", row["o"]))
    stats["shared pairs"] = len(shared)

    components: dict[tuple[str, int], list[tuple[str, int]]] = {}
    for node in list(dsu.parent):
        components.setdefault(dsu.find(node), []).append(node)

    seq = db.execute("SELECT seq FROM main.sqlite_sequence WHERE name = 'entry'").fetchone()
    top = max(seq[0] if seq else 0, max(base_types, default=0))
    offset = (top // 10000 + 1) * 10000
    base_map: dict[int, int] = {}
    other_map: dict[int, int] = {}
    merged_ids: set[int] = set()
    for members in components.values():
        base_ids = sorted(i for side, i in members if side == "b")
        other_ids = sorted(i for side, i in members if side == "o")
        new_id = base_ids[0] if base_ids else other_ids[0] + offset
        types = {base_types[i] for i in base_ids} | {other_types[i] for i in other_ids}
        if len(types) > 1:
            sys.exit(f"entry type conflict in component {members}: {types}")
        for i in base_ids:
            base_map[i] = new_id
        for i in other_ids:
            other_map[i] = new_id
        if len(members) > 1:
            merged_ids.add(new_id)
            stats["merged components"] += 1
            if len(base_ids) > 1:
                stats["components joining >1 base entries"] += 1

    db.execute("CREATE TEMP TABLE base_map (old INTEGER PRIMARY KEY, new INTEGER NOT NULL)")
    db.execute("CREATE TEMP TABLE other_map (old INTEGER PRIMARY KEY, new INTEGER NOT NULL)")
    db.executemany(
        "INSERT INTO base_map VALUES (?, ?)", [(k, v) for k, v in base_map.items() if k != v]
    )
    db.executemany("INSERT INTO other_map VALUES (?, ?)", other_map.items())

    # --- import_run: append the other DB's runs after the base's. -------------
    run_offset = db.execute(
        "SELECT max(coalesce((SELECT seq FROM main.sqlite_sequence WHERE name = 'import_run'), 0), "
        "coalesce((SELECT max(id) FROM main.import_run), 0))"
    ).fetchone()[0]
    db.create_function("run", 1, lambda r: None if r is None else r + run_offset, deterministic=True)
    db.execute(
        "INSERT INTO main.import_run (id, started_at, status, pid) "
        "SELECT run(id), started_at, status, pid FROM o.import_run"
    )

    # --- entry ----------------------------------------------------------------
    stats["base entries merged away"] = db.execute(
        "DELETE FROM main.entry WHERE id IN (SELECT old FROM base_map)"
    ).rowcount
    stats["other entries added"] = db.execute(
        "INSERT INTO main.entry (id, entry_type) "
        "SELECT DISTINCT m.new, e.entry_type FROM o.entry e JOIN other_map m ON m.old = e.id "
        "WHERE m.new NOT IN (SELECT id FROM main.entry)"
    ).rowcount

    # --- entry_source ---------------------------------------------------------
    db.execute(
        "UPDATE main.entry_source SET entry_id = (SELECT new FROM base_map WHERE old = entry_id) "
        "WHERE entry_id IN (SELECT old FROM base_map)"
    )
    cols = (
        "release_date, fetched_at, duration_ms, duration_ms_all, release_type, "
        "num_discs, num_tracks, primary_type, track_extra"
    )
    stats["other pairs added"] = db.execute(
        f"INSERT INTO main.entry_source (source, identifier, entry_id, run_id, {cols}) "
        f"SELECT x.source, x.identifier, m.new, run(x.run_id), {', '.join('x.' + c.strip() for c in cols.split(','))} "
        "FROM o.entry_source x JOIN other_map m ON m.old = x.entry_id "
        "WHERE NOT EXISTS (SELECT 1 FROM main.entry_source b "
        "WHERE b.source = x.source AND b.identifier = x.identifier)"
    ).rowcount

    # Shared pairs: the other row wins when it is fetched and newer. A stub
    # never overwrites; a stub missing a duration takes the other's.
    other_won: set[tuple[str, str]] = set()
    pairs = db.execute(
        "SELECT x.*, b.fetched_at AS b_fetched_at, b.duration_ms AS b_duration_ms "
        "FROM o.entry_source x JOIN main.entry_source b USING (source, identifier)"
    ).fetchall()
    for x in pairs:
        key = (x["source"], x["identifier"])
        base_view = {"fetched_at": x["b_fetched_at"]}
        if source_row_wins(x, base_view):
            other_won.add(key)
            db.execute(
                "UPDATE main.entry_source SET "
                + ", ".join(f"{c.strip()} = ?" for c in cols.split(","))
                + " WHERE source = ? AND identifier = ?",
                [x[c.strip()] for c in cols.split(",")] + list(key),
            )
        elif x["b_fetched_at"] is None and x["b_duration_ms"] is None and x["duration_ms"] is not None:
            stats["stub durations filled"] += 1
            db.execute(
                "UPDATE main.entry_source SET duration_ms = ? WHERE source = ? AND identifier = ?",
                (x["duration_ms"], *key),
            )
    stats["shared pairs taking the other DB's row"] = len(other_won)

    # --- entry_alias / contribution: skip identical rows ----------------------
    stats["aliases added"] = db.execute(
        'INSERT INTO main.entry_alias (source, identifier, name, locale, extra, "primary", run_id) '
        'SELECT x.source, x.identifier, x.name, x.locale, x.extra, x."primary", run(x.run_id) '
        "FROM o.entry_alias x WHERE NOT EXISTS (SELECT 1 FROM main.entry_alias b "
        "WHERE b.source = x.source AND b.identifier = x.identifier AND b.name = x.name "
        "AND b.locale IS x.locale AND coalesce(b.extra, 'null') = coalesce(x.extra, 'null') "
        'AND b."primary" = x."primary") ORDER BY x.id'
    ).rowcount
    stats["contributions added"] = db.execute(
        "INSERT INTO main.contribution "
        "(source, identifier, artist_source, artist_identifier, role, main_artist, extra, run_id) "
        "SELECT x.source, x.identifier, x.artist_source, x.artist_identifier, x.role, "
        "x.main_artist, x.extra, run(x.run_id) FROM o.contribution x "
        "WHERE NOT EXISTS (SELECT 1 FROM main.contribution b "
        "WHERE b.source = x.source AND b.identifier = x.identifier "
        "AND b.artist_source = x.artist_source AND b.artist_identifier = x.artist_identifier "
        "AND b.role = x.role AND b.main_artist = x.main_artist AND b.extra IS x.extra) ORDER BY x.id"
    ).rowcount

    # --- entry_child: idempotent on PK, the winning parent row's edge wins ----
    db.execute("CREATE TEMP TABLE other_won (source TEXT, identifier TEXT, PRIMARY KEY (source, identifier))")
    db.executemany("INSERT INTO other_won VALUES (?, ?)", other_won)
    child_cols = "parent_source, parent_identifier, child_source, child_identifier, disc_no, track_no"
    select_children = (
        f"SELECT {child_cols}, run(run_id) FROM o.entry_child x WHERE "
        "{cond} EXISTS (SELECT 1 FROM other_won w "
        "WHERE w.source = x.parent_source AND w.identifier = x.parent_identifier)"
    )
    stats["child edges added/replaced (other parent won)"] = db.execute(
        f"INSERT OR REPLACE INTO main.entry_child ({child_cols}, run_id) "
        + select_children.format(cond="")
    ).rowcount
    stats["child edges added"] = db.execute(
        f"INSERT OR IGNORE INTO main.entry_child ({child_cols}, run_id) "
        + select_children.format(cond="NOT")
    ).rowcount

    # --- entry_relation: remap, normalize, drop self, combine duplicates ------
    merged: dict[tuple[int, int, str], dict] = {}
    remap = {
        "main": lambda i: base_map.get(i, i),
        "o": lambda i: other_map.get(i, i + offset),
    }
    for side in ("main", "o"):
        for row in db.execute(f"SELECT * FROM {side}.entry_relation"):
            rel = dict(row)
            a, b = remap[side](rel["entry_a"]), remap[side](rel["entry_b"])
            if a == b:
                stats["relations dropped (self after merge)"] += 1
                continue
            rel["entry_a"], rel["entry_b"] = min(a, b), max(a, b)
            key = (rel["entry_a"], rel["entry_b"], rel["kind"])
            if key in merged:
                stats["relations combined"] += 1
                merged[key] = combine_relations(merged[key], rel)
            else:
                merged[key] = rel
    db.execute("DELETE FROM main.entry_relation")
    db.executemany(
        "INSERT INTO main.entry_relation (entry_a, entry_b, kind, confidence, origin, enabled, extra) "
        "VALUES (:entry_a, :entry_b, :kind, :confidence, :origin, :enabled, :extra)",
        merged.values(),
    )
    stats["relations total"] = len(merged)

    # --- dedup_feedback: remap unless it would become a self-pair -------------
    if db.execute("SELECT count(*) FROM o.dedup_feedback").fetchone()[0]:
        sys.exit("the other DB has dedup_feedback rows; merging two ledgers is not supported")
    for row in db.execute(
        "SELECT id, entry_a, entry_b FROM main.dedup_feedback "
        "WHERE entry_a IN (SELECT old FROM base_map) OR entry_b IN (SELECT old FROM base_map)"
    ).fetchall():
        a = base_map.get(row["entry_a"], row["entry_a"])
        b = base_map.get(row["entry_b"], row["entry_b"])
        if a == b:
            stats["feedback rows left unmapped (self after merge)"] += 1
            continue
        stats["feedback rows remapped"] += 1
        db.execute(
            "UPDATE main.dedup_feedback SET entry_a = ?, entry_b = ? WHERE id = ?",
            (min(a, b), max(a, b), row["id"]),
        )

    # --- dedup barrier bookkeeping and block index ----------------------------
    db.execute(
        "UPDATE OR IGNORE main.entry_dedup SET entry_id = (SELECT new FROM base_map WHERE old = entry_id) "
        "WHERE entry_id IN (SELECT old FROM base_map)"
    )
    db.execute("DELETE FROM main.entry_dedup WHERE entry_id IN (SELECT old FROM base_map)")
    db.execute(
        "INSERT OR IGNORE INTO main.entry_dedup (entry_id, config_path) "
        "SELECT m.new, x.config_path FROM o.entry_dedup x JOIN other_map m ON m.old = x.entry_id"
    )
    db.execute("INSERT OR IGNORE INTO main.dedup_state SELECT * FROM o.dedup_state")
    db.execute(
        "INSERT OR IGNORE INTO main.dedup_block_key (entry_id, block_key) "
        "SELECT m.new, x.block_key FROM o.dedup_block_key x JOIN other_map m ON m.old = x.entry_id"
    )
    db.execute("DELETE FROM main.dedup_block_key WHERE entry_id IN (SELECT old FROM base_map)")
    db.execute("CREATE TEMP TABLE merged_ids (id INTEGER PRIMARY KEY)")
    db.executemany("INSERT INTO merged_ids VALUES (?)", ((i,) for i in merged_ids))
    db.execute("DELETE FROM main.dedup_block_key WHERE entry_id IN (SELECT id FROM merged_ids)")

    # --- invariants -----------------------------------------------------------
    checks = {
        "entry_source rows pointing at a missing entry": "SELECT count(*) FROM main.entry_source "
        "WHERE entry_id NOT IN (SELECT id FROM main.entry)",
        "orphan entries": "SELECT count(*) FROM main.entry WHERE id NOT IN "
        "(SELECT entry_id FROM main.entry_source)",
        "relations pointing at a missing entry": "SELECT count(*) FROM main.entry_relation "
        "WHERE entry_a NOT IN (SELECT id FROM main.entry) OR entry_b NOT IN (SELECT id FROM main.entry)",
        "pair count mismatch": "SELECT (SELECT count(*) FROM main.entry_source) - (SELECT count(*) FROM "
        "(SELECT source, identifier FROM main.entry_source UNION SELECT source, identifier FROM o.entry_source))",
    }
    failed = False
    for label, sql in checks.items():
        n = db.execute(sql).fetchone()[0]
        if n:
            print(f"INVARIANT FAILED: {label}: {n}", file=sys.stderr)
            failed = True
    if failed:
        db.execute("ROLLBACK")
        db.close()
        out_path.unlink()
        sys.exit(1)

    db.execute("COMMIT")
    db.execute("DETACH DATABASE o")
    db.execute("INSERT INTO entry_alias_fts(entry_alias_fts) VALUES ('integrity-check')")
    integrity = db.execute("PRAGMA integrity_check").fetchone()[0]
    stats["entries total"] = db.execute("SELECT count(*) FROM entry").fetchone()[0]
    stats["pairs total"] = db.execute("SELECT count(*) FROM entry_source").fetchone()[0]
    db.close()

    print(f"entry id offset for the other DB: {offset}; run id offset: {run_offset}")
    for key, value in stats.items():
        print(f"{key}: {value}")
    print(f"integrity_check: {integrity}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("base", type=Path, help="DB whose entry ids are preserved")
    parser.add_argument("other", type=Path, help="DB folded into the base")
    parser.add_argument("out", type=Path, help="new merged DB (must not exist)")
    args = parser.parse_args()
    merge(args.base, args.other, args.out)


if __name__ == "__main__":
    main()
