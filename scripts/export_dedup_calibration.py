#!/usr/bin/env python3
"""Export a blind, provenance-bearing deduplication calibration batch.

The exporter intentionally does not assign expected labels. It samples several
frames from a musiclib SQLite snapshot and writes two JSONL files:

* private tasks, containing selection and masked-bridge provenance;
* blind tasks, with that private metadata physically removed.

The first calibration pass is for settling the identity ontology, not for
estimating production metrics. Later evaluation exports should retain the same
record format but use a pre-registered sampling design.
"""

from __future__ import annotations

import argparse
import copy
import datetime as dt
import difflib
import hashlib
import itertools
import json
import random
import re
import sqlite3
import unicodedata
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any, Iterable


SCHEMA_VERSION = "dedup-calibration/2"
ONTOLOGY_VERSION = "identity-primitives-v2"
POLICY_VERSION = "library-policy-v2"

SOURCE_PRIORITY = {
    "musicbrainz": 0,
    "spotify": 1,
    "discogs": 2,
    "youtube": 3,
    "soundcloud": 4,
    "nicovideo": 5,
    "local": 6,
    "isrc": 7,
    "upc": 8,
    "apple_music": 9,
    "deezer": 10,
    "tidal": 11,
    "lastfm": 12,
    "vgmdb": 13,
    "unknown_url": 99,
}

ENTRY_TYPES = {"track", "release", "release_group", "artist"}


def canonical_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def sha256_value(value: Any) -> str:
    return "sha256:" + hashlib.sha256(canonical_json(value).encode("utf-8")).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def rfc3339_from_unix(value: int | None) -> str | None:
    if value is None:
        return None
    return dt.datetime.fromtimestamp(value, tz=dt.timezone.utc).isoformat().replace("+00:00", "Z")


def normalize_name(text: str) -> str:
    text = unicodedata.normalize("NFKC", text).casefold()
    return " ".join("".join(ch if ch.isalnum() else " " for ch in text).split())


def script_class(text: str | None) -> str:
    if not text:
        return "missing"
    has_latin = any("LATIN" in unicodedata.name(ch, "") for ch in text)
    has_cjk = any(
        "CJK" in unicodedata.name(ch, "")
        or "HIRAGANA" in unicodedata.name(ch, "")
        or "KATAKANA" in unicodedata.name(ch, "")
        or "HANGUL" in unicodedata.name(ch, "")
        for ch in text
    )
    if has_latin and has_cjk:
        return "mixed"
    if has_cjk:
        return "cjk"
    if has_latin:
        return "latin"
    return "other"


def script_relation(left: str | None, right: str | None) -> str:
    a, b = script_class(left), script_class(right)
    if {a, b} == {"cjk", "latin"}:
        return "cjk_latin"
    if "mixed" in {a, b}:
        return "mixed"
    if a == b:
        return a
    return "other"


def parse_json_or_raw(value: str | None) -> Any:
    if value is None:
        return None
    try:
        return json.loads(value)
    except json.JSONDecodeError:
        return value


def prefix_evidence_ids(records: list[dict[str, Any]], side: str) -> list[dict[str, Any]]:
    records = copy.deepcopy(records)
    for record_number, record in enumerate(records, 1):
        for fact in record["facts"]:
            fact["evidence_id"] = f"{side}.{record_number}.{fact['evidence_id']}"
    return records


class Snapshot:
    def __init__(self, db_path: Path, max_records_per_view: int):
        self.db_path = db_path
        self.max_records_per_view = max_records_per_view
        uri = f"file:{db_path.resolve()}?mode=ro"
        self.db = sqlite3.connect(uri, uri=True)
        self.db.row_factory = sqlite3.Row

        self.entry_types: dict[int, str] = {
            int(row["id"]): str(row["entry_type"])
            for row in self.db.execute("SELECT id, entry_type FROM entry ORDER BY id")
            if row["entry_type"] in ENTRY_TYPES
        }
        self.sources_by_entry: dict[int, list[sqlite3.Row]] = defaultdict(list)
        self.entry_by_pair: dict[tuple[str, str], int] = {}
        for row in self.db.execute("SELECT * FROM entry_source ORDER BY entry_id, source, identifier"):
            entry_id = int(row["entry_id"])
            if entry_id not in self.entry_types:
                continue
            self.sources_by_entry[entry_id].append(row)
            self.entry_by_pair[(str(row["source"]), str(row["identifier"]))] = entry_id

        self.aliases_by_pair: dict[tuple[str, str], list[sqlite3.Row]] = defaultdict(list)
        for row in self.db.execute(
            'SELECT * FROM entry_alias ORDER BY source, identifier, "primary" DESC, name, id'
        ):
            self.aliases_by_pair[(str(row["source"]), str(row["identifier"]))].append(row)

        self.contribs_by_pair: dict[tuple[str, str], list[sqlite3.Row]] = defaultdict(list)
        for row in self.db.execute(
            "SELECT * FROM contribution ORDER BY source, identifier, main_artist DESC, role, id"
        ):
            self.contribs_by_pair[(str(row["source"]), str(row["identifier"]))].append(row)

        self.parents_by_child: dict[tuple[str, str], list[sqlite3.Row]] = defaultdict(list)
        self.children_by_parent: dict[tuple[str, str], list[sqlite3.Row]] = defaultdict(list)
        for row in self.db.execute(
            "SELECT * FROM entry_child ORDER BY child_source, child_identifier, "
            "parent_source, parent_identifier, disc_no, track_no"
        ):
            self.parents_by_child[(str(row["child_source"]), str(row["child_identifier"]))].append(row)
            self.children_by_parent[(str(row["parent_source"]), str(row["parent_identifier"]))].append(row)

        self._record_cache: dict[tuple[str, str], dict[str, Any]] = {}
        self._entry_view_cache: dict[int, dict[str, Any]] = {}
        self._display_cache: dict[int, str | None] = {}
        self._names_cache: dict[int, set[str]] = {}
        self._durations_cache: dict[int, set[int]] = {}
        self._artists_cache: dict[int, set[str]] = {}
        self._tracklist_cache: dict[int, set[str]] = {}

    def close(self) -> None:
        self.db.close()

    def display_title(self, entry_id: int) -> str | None:
        if entry_id in self._display_cache:
            return self._display_cache[entry_id]
        aliases: list[tuple[int, int, str]] = []
        for source_row in self.sources_by_entry.get(entry_id, []):
            key = (str(source_row["source"]), str(source_row["identifier"]))
            priority = SOURCE_PRIORITY.get(key[0], 50)
            for alias in self.aliases_by_pair.get(key, []):
                name = str(alias["name"]).strip()
                if name:
                    aliases.append((0 if alias["primary"] else 1, priority, name))
        title = min(aliases)[2] if aliases else None
        self._display_cache[entry_id] = title
        return title

    def normalized_names(self, entry_id: int) -> set[str]:
        if entry_id not in self._names_cache:
            self._names_cache[entry_id] = {
                norm
                for row in self.sources_by_entry.get(entry_id, [])
                for alias in self.aliases_by_pair.get((str(row["source"]), str(row["identifier"])), [])
                if (norm := normalize_name(str(alias["name"])))
            }
        return self._names_cache[entry_id]

    def durations(self, entry_id: int) -> set[int]:
        if entry_id not in self._durations_cache:
            values: set[int] = set()
            for row in self.sources_by_entry.get(entry_id, []):
                parsed = parse_json_or_raw(row["duration_ms_all"])
                if isinstance(parsed, list):
                    values.update(int(value) for value in parsed if value is not None)
                elif row["duration_ms"] is not None:
                    values.add(int(row["duration_ms"]))
            self._durations_cache[entry_id] = values
        return self._durations_cache[entry_id]

    def artist_names(self, entry_id: int) -> set[str]:
        if entry_id not in self._artists_cache:
            names: set[str] = set()
            for row in self.sources_by_entry.get(entry_id, []):
                pair = (str(row["source"]), str(row["identifier"]))
                for contribution in self.contribs_by_pair.get(pair, []):
                    artist_pair = (
                        str(contribution["artist_source"]),
                        str(contribution["artist_identifier"]),
                    )
                    artist_entry = self.entry_by_pair.get(artist_pair)
                    name = self.display_title(artist_entry) if artist_entry is not None else None
                    extra = parse_json_or_raw(contribution["extra"])
                    if name is None and isinstance(extra, dict):
                        name = extra.get("artist_name")
                    if name and (norm := normalize_name(str(name))):
                        names.add(norm)
            self._artists_cache[entry_id] = names
        return self._artists_cache[entry_id]

    def tracklist_titles(self, entry_id: int) -> set[str]:
        if entry_id not in self._tracklist_cache:
            titles: set[str] = set()
            for row in self.sources_by_entry.get(entry_id, []):
                pair = (str(row["source"]), str(row["identifier"]))
                for child in self.children_by_parent.get(pair, []):
                    child_pair = (str(child["child_source"]), str(child["child_identifier"]))
                    child_entry = self.entry_by_pair.get(child_pair)
                    title = self.display_title(child_entry) if child_entry is not None else None
                    if title and (norm := normalize_name(title)):
                        titles.add(norm)
            self._tracklist_cache[entry_id] = titles
        return self._tracklist_cache[entry_id]

    def record(self, pair: tuple[str, str]) -> dict[str, Any]:
        if pair in self._record_cache:
            return self._record_cache[pair]
        entry_id = self.entry_by_pair[pair]
        row = next(
            item
            for item in self.sources_by_entry[entry_id]
            if (str(item["source"]), str(item["identifier"])) == pair
        )
        facts: list[dict[str, Any]] = []

        def add(field: str, value: Any) -> None:
            number = 1 + sum(1 for fact in facts if fact["field"] == field)
            facts.append({"evidence_id": f"{field}.{number}", "field": field, "value": value})

        for alias in self.aliases_by_pair.get(pair, []):
            value: dict[str, Any] = {
                "name": str(alias["name"]),
                "locale": alias["locale"],
                "primary": bool(alias["primary"]),
            }
            extra = parse_json_or_raw(alias["extra"])
            if extra not in (None, {}, []):
                value["extra"] = extra
            add("alias", value)

        durations = parse_json_or_raw(row["duration_ms_all"])
        if not isinstance(durations, list):
            durations = [row["duration_ms"]] if row["duration_ms"] is not None else []
        for duration in sorted({int(value) for value in durations if value is not None}):
            add("duration_ms", duration)
        if row["release_date"] is not None:
            add("release_date", str(row["release_date"]))
        if row["release_type"] is not None:
            vocabulary = {
                "discogs": "discogs_release_status",
                "spotify": "spotify_album_type",
                "local": "declared_release_type",
                "youtube_api": "collection_type",
                "youtube": "collection_type",
                "soundcloud": "collection_type",
                "nicovideo": "collection_type",
            }.get(pair[0], "provider_release_type")
            add(
                "source_classification",
                {"vocabulary": vocabulary, "value": str(row["release_type"])},
            )
        if row["primary_type"] is not None:
            add("primary_type", str(row["primary_type"]))

        for contribution in self.contribs_by_pair.get(pair, []):
            artist_pair = (
                str(contribution["artist_source"]),
                str(contribution["artist_identifier"]),
            )
            artist_entry = self.entry_by_pair.get(artist_pair)
            # Despite the historical column names, these rows store an edge
            # child. Only artist children are artist credits; release-track
            # children are represented by entry_child and handled below.
            if artist_entry is None or self.entry_types.get(artist_entry) != "artist":
                continue
            value = {
                "record_id": f"{artist_pair[0]}:{artist_pair[1]}",
                "name": self.display_title(artist_entry),
                "role": str(contribution["role"]),
                "main": bool(contribution["main_artist"]),
            }
            extra = parse_json_or_raw(contribution["extra"])
            if extra not in (None, {}, []):
                value["extra"] = extra
            add("artist_credit", value)

        for parent in self.parents_by_child.get(pair, []):
            parent_pair = (str(parent["parent_source"]), str(parent["parent_identifier"]))
            parent_entry = self.entry_by_pair.get(parent_pair)
            parent_type = self.entry_types.get(parent_entry) if parent_entry is not None else None
            value = {
                "record_id": f"{parent_pair[0]}:{parent_pair[1]}",
                "entry_type": parent_type,
                "title": self.display_title(parent_entry) if parent_entry is not None else None,
            }
            current_type = self.entry_types[entry_id]
            if current_type == "track" and parent_type == "release":
                add("parent_release", value)
                add(
                    "track_position",
                    {"disc_no": parent["disc_no"], "track_no": parent["track_no"]},
                )
            elif current_type == "release" and parent_type == "release_group":
                add("parent_release_group", value)
            elif current_type == "artist":
                add("credited_on", value)
            else:
                add("parent_entity", value)

        # Tracklists are valuable release evidence, but cap pathological
        # playlist-like parents so a single task cannot consume a whole prompt.
        children = sorted(
            self.children_by_parent.get(pair, []),
            key=lambda child: (
                child["disc_no"] is None,
                child["disc_no"] or 0,
                child["track_no"] is None,
                child["track_no"] or 0,
                child["child_source"],
                child["child_identifier"],
            ),
        )
        for child in children[:100]:
            child_pair = (str(child["child_source"]), str(child["child_identifier"]))
            child_entry = self.entry_by_pair.get(child_pair)
            add(
                "tracklist_item",
                {
                    "record_id": f"{child_pair[0]}:{child_pair[1]}",
                    "title": self.display_title(child_entry) if child_entry is not None else None,
                    "disc_no": child["disc_no"],
                    "track_no": child["track_no"],
                },
            )

        if pair[0] == "unknown_url" or re.match(r"^https?://", pair[1]):
            add("external_link", pair[1])

        generic = {
            "record_id": f"{pair[0]}:{pair[1]}",
            "source": pair[0],
            "identifier": pair[1],
            "external_type": None,
            "fetched_at": rfc3339_from_unix(int(row["fetched_at"])),
            "facts": facts,
        }
        generic["evidence_sha256"] = sha256_value(generic)
        self._record_cache[pair] = generic
        return generic

    def view(self, entry_id: int, pairs: Iterable[tuple[str, str]] | None = None) -> dict[str, Any]:
        if pairs is None and entry_id in self._entry_view_cache:
            return self._entry_view_cache[entry_id]
        if pairs is None:
            chosen_pairs = [
                (str(row["source"]), str(row["identifier"]))
                for row in self.sources_by_entry[entry_id]
            ]
        else:
            chosen_pairs = sorted(set(pairs))
        records = [self.record(pair) for pair in chosen_pairs]
        records.sort(
            key=lambda record: (
                0 if any(fact["field"] == "alias" for fact in record["facts"]) else 1,
                SOURCE_PRIORITY.get(record["source"], 50),
                record["source"],
                record["identifier"],
            )
        )
        records_total = len(records)
        records = records[: self.max_records_per_view]
        display = next(
            (
                fact["value"]["name"]
                for record in records
                for fact in record["facts"]
                if fact["field"] == "alias" and fact["value"].get("primary")
            ),
            None,
        )
        if display is None:
            display = next(
                (
                    fact["value"]["name"]
                    for record in records
                    for fact in record["facts"]
                    if fact["field"] == "alias"
                ),
                None,
            )
        generic = {
            "entry_type": self.entry_types[entry_id],
            "display_title": display,
            "records_total": records_total,
            "records_omitted": records_total - len(records),
            "records": records,
        }
        view_id = sha256_value(generic)
        view = {"view_id": view_id, **generic}
        if pairs is None:
            self._entry_view_cache[entry_id] = view
        return view


def public_view(view: dict[str, Any], side: str) -> dict[str, Any]:
    result = copy.deepcopy(view)
    result["records"] = prefix_evidence_ids(result["records"], side)
    return result


def make_candidate(
    snapshot_id: str,
    created_at: str,
    left: dict[str, Any],
    right: dict[str, Any],
    hidden: dict[str, Any],
) -> dict[str, Any] | None:
    if left["view_id"] == right["view_id"]:
        return None
    left, right = sorted([left, right], key=lambda view: view["view_id"])
    entry_type = left["entry_type"]
    if entry_type != right["entry_type"]:
        return None
    item_id = sha256_value(
        [
            SCHEMA_VERSION,
            snapshot_id,
            ONTOLOGY_VERSION,
            POLICY_VERSION,
            entry_type,
            left["view_id"],
            right["view_id"],
        ]
    )
    return {
        "record_type": "task",
        "schema_version": SCHEMA_VERSION,
        "item_id": item_id,
        "snapshot_id": snapshot_id,
        "ontology_version": ONTOLOGY_VERSION,
        "policy_version": POLICY_VERSION,
        "created_at": created_at,
        "entry_type": entry_type,
        "left": public_view(left, "L"),
        "right": public_view(right, "R"),
        "hidden": hidden,
    }


def masked_bridge_candidates(snapshot: Snapshot, snapshot_id: str, created_at: str) -> list[dict[str, Any]]:
    output: list[dict[str, Any]] = []
    for entry_id, source_rows in snapshot.sources_by_entry.items():
        pairs = [(str(row["source"]), str(row["identifier"])) for row in source_rows]
        cross_source = [pair for pair in itertools.combinations(pairs, 2) if pair[0][0] != pair[1][0]]
        # A single very large artist cluster must not dominate the eligible pool.
        cross_source.sort(key=lambda pair: sha256_value([snapshot_id, entry_id, pair]))
        for left_pair, right_pair in cross_source[:30]:
            left = snapshot.view(entry_id, [left_pair])
            right = snapshot.view(entry_id, [right_pair])
            relation = script_relation(left["display_title"], right["display_title"])
            source_pair = "-".join(sorted([left_pair[0], right_pair[0]]))
            hidden = {
                "sampling_frame": "masked_bridge",
                "stratum": f"{left['entry_type']}/{relation}/{source_pair}",
                "eligible_count": 1,
                "selected_count": 1,
                "selection_method": "bounded_pool_then_stratified_uniform",
                "inclusion_probability": None,
                "production_candidate": None,
                "candidate_set_version": None,
                "proposers": [
                    {"name": "current_cluster_split", "version": SCHEMA_VERSION, "rank": None,
                     "score": None, "score_name": None}
                ],
                "masked_links": [
                    {"kind": "current_cluster", "value_hash": sha256_value([snapshot_id, entry_id])}
                ],
                "current_entry_ids": [entry_id, entry_id],
                "split_group_keys": [f"local-entry:{entry_id}"],
                "partition": "calibration",
            }
            candidate = make_candidate(snapshot_id, created_at, left, right, hidden)
            if candidate is not None:
                output.append(candidate)
    return output


def hard_confuser_candidates(snapshot: Snapshot, snapshot_id: str, created_at: str) -> list[dict[str, Any]]:
    entries_by_name: dict[tuple[str, str], set[int]] = defaultdict(set)
    for pair, aliases in snapshot.aliases_by_pair.items():
        entry_id = snapshot.entry_by_pair.get(pair)
        if entry_id not in snapshot.entry_types:
            continue
        for alias in aliases:
            norm = normalize_name(str(alias["name"]))
            if len(norm) >= 2:
                entries_by_name[(snapshot.entry_types[entry_id], norm)].add(entry_id)

    output: list[dict[str, Any]] = []
    for (entry_type, norm), entry_ids in entries_by_name.items():
        if not 2 <= len(entry_ids) <= 20:
            continue
        pairs = list(itertools.combinations(sorted(entry_ids), 2))
        pairs.sort(key=lambda pair: sha256_value([snapshot_id, norm, pair]))
        for left_id, right_id in pairs[:30]:
            left, right = snapshot.view(left_id), snapshot.view(right_id)
            relation = script_relation(left["display_title"], right["display_title"])
            hidden = {
                "sampling_frame": "hard_confuser",
                "stratum": f"{entry_type}/{relation}/exact_alias_collision",
                "eligible_count": 1,
                "selected_count": 1,
                "selection_method": "bounded_pool_then_stratified_uniform",
                "inclusion_probability": None,
                "production_candidate": None,
                "candidate_set_version": None,
                "proposers": [
                    {"name": "exact_alias_collision", "version": "nfkc-casefold-v1", "rank": None,
                     "score": None, "score_name": None}
                ],
                "masked_links": [],
                "current_entry_ids": [left_id, right_id],
                "split_group_keys": [f"name:{sha256_value(norm)}"],
                "partition": "calibration",
            }
            candidate = make_candidate(snapshot_id, created_at, left, right, hidden)
            if candidate is not None:
                output.append(candidate)
    return output


def character_grams(text: str) -> set[str]:
    compact = text.replace(" ", "")
    if len(compact) <= 3:
        return {compact} if compact else set()
    return {compact[index : index + 3] for index in range(len(compact) - 2)}


def best_name_similarity(left: set[str], right: set[str]) -> tuple[float, float]:
    sequence = 0.0
    token = 0.0
    for a in left:
        a_tokens = set(a.split())
        for b in right:
            sequence = max(sequence, difflib.SequenceMatcher(None, a, b).ratio())
            b_tokens = set(b.split())
            union = a_tokens | b_tokens
            if union:
                token = max(token, len(a_tokens & b_tokens) / len(union))
    return sequence, token


def independent_miss_candidates(
    snapshot: Snapshot,
    snapshot_id: str,
    created_at: str,
) -> list[dict[str, Any]]:
    """Generate bounded candidates between complete, distinct entry views.

    These channels deliberately operate on current entries, never by splitting
    the source records already grouped inside one entry.
    """
    proposals: dict[tuple[int, int], dict[str, float]] = defaultdict(dict)
    ids = sorted(snapshot.entry_types)

    # Lexical blocking: rare character trigrams followed by an actual string
    # similarity check. Per-entry top-k bounds prolific aliases and generic names.
    gram_index: dict[tuple[str, str], set[int]] = defaultdict(set)
    for entry_id in ids:
        entry_type = snapshot.entry_types[entry_id]
        for name in snapshot.normalized_names(entry_id):
            for gram in character_grams(name):
                gram_index[(entry_type, gram)].add(entry_id)
    lexical_neighbors: dict[int, set[int]] = defaultdict(set)
    for bucket in gram_index.values():
        if 2 <= len(bucket) <= 50:
            for left_id in bucket:
                lexical_neighbors[left_id].update(bucket - {left_id})
    for left_id, neighbors in lexical_neighbors.items():
        ranked: list[tuple[float, float, int]] = []
        left_names = snapshot.normalized_names(left_id)
        for right_id in neighbors:
            if right_id <= left_id:
                continue
            right_names = snapshot.normalized_names(right_id)
            if left_names & right_names:  # exact collisions have their own frame
                continue
            sequence, token = best_name_similarity(left_names, right_names)
            if sequence >= 0.58 or token >= 0.50:
                ranked.append((max(sequence, token), sequence, right_id))
        for score, _, right_id in sorted(ranked, reverse=True)[:12]:
            proposals[(left_id, right_id)]["bounded_char_trigram"] = score

    # Track structure: an exact credited-artist hypothesis and a nearby duration
    # are a strong block even when titles use different scripts or translations.
    duration_artist: dict[tuple[str, int], set[int]] = defaultdict(set)
    for entry_id in ids:
        if snapshot.entry_types[entry_id] != "track":
            continue
        durations = snapshot.durations(entry_id)
        artists = snapshot.artist_names(entry_id)
        for artist in artists:
            for duration in durations:
                duration_artist[(artist, round(duration / 5000))].add(entry_id)
    structural_pairs: set[tuple[int, int]] = set()
    for bucket in duration_artist.values():
        if 2 <= len(bucket) <= 80:
            structural_pairs.update(itertools.combinations(sorted(bucket), 2))
    for left_id, right_id in structural_pairs:
        if snapshot.normalized_names(left_id) & snapshot.normalized_names(right_id):
            continue
        left_durations = snapshot.durations(left_id)
        right_durations = snapshot.durations(right_id)
        if not left_durations or not right_durations:
            continue
        delta = min(abs(a - b) for a in left_durations for b in right_durations)
        if delta > 6000:
            continue
        sequence, token = best_name_similarity(
            snapshot.normalized_names(left_id), snapshot.normalized_names(right_id)
        )
        if sequence >= 0.30 or token > 0:
            score = 1.0 - min(delta, 6000) / 6000
            proposals[(left_id, right_id)]["duration_credit_block"] = score

    # Release structure: independently modeled releases sharing several track
    # titles are useful duplicate/edition confusers even with dissimilar titles.
    releases_by_track: dict[str, set[int]] = defaultdict(set)
    for entry_id in ids:
        if snapshot.entry_types[entry_id] == "release":
            for title in snapshot.tracklist_titles(entry_id):
                releases_by_track[title].add(entry_id)
    overlaps: Counter[tuple[int, int]] = Counter()
    for bucket in releases_by_track.values():
        if 2 <= len(bucket) <= 30:
            overlaps.update(itertools.combinations(sorted(bucket), 2))
    for (left_id, right_id), shared in overlaps.items():
        if shared < 2:
            continue
        left_titles = snapshot.tracklist_titles(left_id)
        right_titles = snapshot.tracklist_titles(right_id)
        union = left_titles | right_titles
        score = shared / len(union) if union else 0.0
        if score >= 0.18:
            proposals[(left_id, right_id)]["tracklist_overlap"] = score

    output: list[dict[str, Any]] = []
    for (left_id, right_id), scores in sorted(proposals.items()):
        if left_id == right_id:
            raise AssertionError("entry-level candidate generator produced a self-pair")
        left, right = snapshot.view(left_id), snapshot.view(right_id)
        relation = script_relation(left["display_title"], right["display_title"])
        best_channel = max(scores, key=scores.get)
        hidden = {
            "sampling_frame": "independent_miss",
            "stratum": f"{left['entry_type']}/{relation}/{best_channel}",
            "eligible_count": 1,
            "selected_count": 1,
            "selection_method": "bounded_retrieval_then_stratified_uniform",
            "inclusion_probability": None,
            "production_candidate": None,
            "candidate_set_version": "entry-retrieval-v3",
            "proposers": [
                {
                    "name": name,
                    "version": "entry-retrieval-v3",
                    "rank": None,
                    "score": score,
                    "score_name": "retrieval_score",
                }
                for name, score in sorted(scores.items())
            ],
            "masked_links": [],
            "current_entry_ids": [left_id, right_id],
            "split_group_keys": [f"local-entry:{left_id}", f"local-entry:{right_id}"],
            "partition": "calibration",
        }
        candidate = make_candidate(snapshot_id, created_at, left, right, hidden)
        if candidate is not None:
            output.append(candidate)
    return output


def production_candidates(
    snapshot: Snapshot,
    snapshot_id: str,
    created_at: str,
    path: Path | None,
) -> tuple[list[dict[str, Any]], str | None]:
    if path is None or not path.exists():
        return [], None
    candidate_version = sha256_file(path)
    with path.open(encoding="utf-8") as handle:
        legacy = json.load(handle)
    output: list[dict[str, Any]] = []
    for row in legacy.values():
        try:
            left_id, right_id = int(row["entry_a"]), int(row["entry_b"])
        except (KeyError, TypeError, ValueError):
            continue
        if left_id not in snapshot.entry_types or right_id not in snapshot.entry_types:
            continue
        left, right = snapshot.view(left_id), snapshot.view(right_id)
        relation = script_relation(left["display_title"], right["display_title"])
        hidden = {
            "sampling_frame": "production_candidate",
            "stratum": f"{left['entry_type']}/{relation}/legacy_softmatch",
            "eligible_count": 1,
            "selected_count": 1,
            "selection_method": "stratified_uniform_without_replacement",
            "inclusion_probability": None,
            "production_candidate": True,
            "candidate_set_version": candidate_version,
            # Deliberately do not copy the old verdict, score, label, or rationale.
            "proposers": [
                {"name": "legacy_softmatch", "version": candidate_version, "rank": None,
                 "score": None, "score_name": None}
            ],
            "masked_links": [],
            "current_entry_ids": [left_id, right_id],
            "split_group_keys": [f"local-entry:{left_id}", f"local-entry:{right_id}"],
            "partition": "calibration",
        }
        candidate = make_candidate(snapshot_id, created_at, left, right, hidden)
        if candidate is not None:
            output.append(candidate)
    return output, candidate_version


def uniform_candidates(
    snapshot: Snapshot,
    snapshot_id: str,
    created_at: str,
    rng: random.Random,
) -> list[dict[str, Any]]:
    by_type: dict[str, list[int]] = defaultdict(list)
    for entry_id, entry_type in snapshot.entry_types.items():
        by_type[entry_type].append(entry_id)
    output: list[dict[str, Any]] = []
    seen: set[tuple[int, int]] = set()
    for entry_type, entry_ids in by_type.items():
        entry_ids.sort()
        if len(entry_ids) < 2:
            continue
        attempts = min(5000, len(entry_ids) * 20)
        for _ in range(attempts):
            left_id, right_id = sorted(rng.sample(entry_ids, 2))
            if (left_id, right_id) in seen:
                continue
            seen.add((left_id, right_id))
            left, right = snapshot.view(left_id), snapshot.view(right_id)
            relation = script_relation(left["display_title"], right["display_title"])
            hidden = {
                "sampling_frame": "uniform",
                "stratum": f"{entry_type}/{relation}/same_type",
                "eligible_count": len(entry_ids) * (len(entry_ids) - 1) // 2,
                "selected_count": 1,
                "selection_method": "random_pool_then_stratified_uniform",
                "inclusion_probability": None,
                "production_candidate": False,
                "candidate_set_version": None,
                "proposers": [],
                "masked_links": [],
                "current_entry_ids": [left_id, right_id],
                "split_group_keys": [f"local-entry:{left_id}", f"local-entry:{right_id}"],
                "partition": "calibration",
            }
            candidate = make_candidate(snapshot_id, created_at, left, right, hidden)
            if candidate is not None:
                output.append(candidate)
    return output


def source_pairs_from_db(path: Path | None) -> set[tuple[str, str]]:
    if path is None or not path.exists():
        return set()
    uri = f"file:{path.resolve()}?mode=ro"
    with sqlite3.connect(uri, uri=True) as db:
        return {
            (str(source), str(identifier))
            for source, identifier in db.execute("SELECT source, identifier FROM entry_source")
        }


def excluded_entry_pairs(paths: Iterable[Path]) -> set[tuple[int, int]]:
    output: set[tuple[int, int]] = set()
    for path in paths:
        if not path.exists():
            continue
        with path.open(encoding="utf-8") as handle:
            for line in handle:
                if not line.strip():
                    continue
                task = json.loads(line)
                ids = task.get("hidden", {}).get("current_entry_ids", [])
                if len(ids) == 2 and ids[0] != ids[1]:
                    output.add(tuple(sorted((int(ids[0]), int(ids[1])))))
    return output


def tag_candidate_origins(
    candidates: Iterable[dict[str, Any]],
    snapshot: Snapshot,
    baseline_pairs: set[tuple[str, str]],
    playlist_pairs: set[tuple[str, str]],
) -> None:
    def origin(entry_id: int) -> str:
        pairs = {
            (str(row["source"]), str(row["identifier"]))
            for row in snapshot.sources_by_entry[entry_id]
        }
        if snapshot.entry_types[entry_id] == "track" and pairs & playlist_pairs:
            return "playlist"
        if pairs & baseline_pairs:
            return "baseline"
        return "expansion"

    for task in candidates:
        entry_ids = task["hidden"].get("current_entry_ids", [])
        if len(entry_ids) != 2 or entry_ids[0] == entry_ids[1]:
            raise ValueError(
                f"entry-pair dataset requires two distinct entry ids: {task['item_id']} {entry_ids}"
            )
        origin_pair = "-".join(sorted(origin(int(entry_id)) for entry_id in entry_ids))
        task["hidden"]["stratum"] += f"/{origin_pair}"


def coarse_stratum(task: dict[str, Any]) -> str:
    hidden = task["hidden"]
    parts = hidden["stratum"].split("/")
    return "/".join(parts[:2])


def sampling_stratum(task: dict[str, Any]) -> str:
    # Include retrieval channel and corpus origin in the sampling bucket. This
    # prevents a large expansion root from dominating merely because it created
    # the most eligible pairs.
    return task["hidden"]["stratum"]


def stratified_sample(
    candidates: list[dict[str, Any]],
    quota: int,
    rng: random.Random,
    excluded_item_ids: set[str] | None = None,
) -> list[dict[str, Any]]:
    excluded_item_ids = excluded_item_ids or set()
    unique = {
        task["item_id"]: task
        for task in candidates
        if task["item_id"] not in excluded_item_ids
    }
    buckets: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for task in unique.values():
        buckets[sampling_stratum(task)].append(task)
    for bucket in buckets.values():
        rng.shuffle(bucket)

    selected: list[dict[str, Any]] = []
    keys = sorted(buckets)
    while len(selected) < quota and keys:
        next_keys: list[str] = []
        for key in keys:
            bucket = buckets[key]
            if bucket and len(selected) < quota:
                selected.append(bucket.pop())
            if bucket:
                next_keys.append(key)
        keys = next_keys

    eligible_counts = Counter(sampling_stratum(task) for task in unique.values())
    selected_counts = Counter(sampling_stratum(task) for task in selected)
    for task in selected:
        key = sampling_stratum(task)
        task["hidden"]["eligible_count"] = eligible_counts[key]
        task["hidden"]["selected_count"] = selected_counts[key]
        if task["hidden"]["selection_method"] == "stratified_uniform_without_replacement":
            task["hidden"]["inclusion_probability"] = selected_counts[key] / eligible_counts[key]
        else:
            # These frames first create a bounded/random calibration pool. The
            # end-to-end source-universe probability is therefore not the
            # selected/pool ratio and must not be presented as one.
            task["hidden"]["inclusion_probability"] = None
    return selected


def blind_projection(task: dict[str, Any]) -> dict[str, Any]:
    task = copy.deepcopy(task)
    task.pop("hidden", None)
    return task


def write_jsonl(path: Path, records: Iterable[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        for record in records:
            handle.write(canonical_json(record) + "\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--db",
        type=Path,
        default=Path.home() / ".local/share/musiclib-rs/musiclib.db",
        help="read-only musiclib SQLite snapshot",
    )
    parser.add_argument(
        "--legacy-candidates",
        type=Path,
        default=Path("data/softmatch_labels_new.json"),
        help="legacy candidate map; verdicts and labels are never exported",
    )
    parser.add_argument("--private-out", type=Path, default=Path("data/dedup-calibration.private.jsonl"))
    parser.add_argument("--blind-out", type=Path, default=Path("data/dedup-calibration.blind.jsonl"))
    parser.add_argument(
        "--manifest-out",
        type=Path,
        default=None,
        help="optional JSON manifest recording corpus inputs and selected strata",
    )
    parser.add_argument("--seed", type=int, default=20260910)
    parser.add_argument("--production", type=int, default=0)
    parser.add_argument(
        "--masked-bridge",
        type=int,
        default=0,
        help="deprecated source-linker diagnostic; keep zero for entry dedup datasets",
    )
    parser.add_argument("--hard-confuser", type=int, default=160)
    parser.add_argument("--independent-miss", type=int, default=200)
    parser.add_argument("--uniform", type=int, default=40)
    parser.add_argument(
        "--baseline-db",
        type=Path,
        default=Path.home() / ".local/share/musiclib-rs/musiclib.db",
        help="optional pre-expansion DB used only to stratify corpus origin",
    )
    parser.add_argument(
        "--playlist-db",
        type=Path,
        default=None,
        help="optional playlist-only DB used only to mark anchor tracks",
    )
    parser.add_argument(
        "--exclude-task-ledger",
        type=Path,
        action="append",
        default=[],
        help="private task JSONL whose distinct entry pairs must not be resampled; repeatable",
    )
    parser.add_argument("--max-records-per-view", type=int, default=20)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if not args.db.exists():
        raise SystemExit(f"musiclib database not found: {args.db}")
    if args.max_records_per_view < 1:
        raise SystemExit("--max-records-per-view must be positive")

    rng = random.Random(args.seed)
    snapshot_id = sha256_file(args.db)
    created_at = dt.datetime.now(tz=dt.timezone.utc).isoformat().replace("+00:00", "Z")
    snapshot = Snapshot(args.db, args.max_records_per_view)
    try:
        frames: list[tuple[str, list[dict[str, Any]], int]] = []
        production, _ = production_candidates(
            snapshot, snapshot_id, created_at, args.legacy_candidates
        )
        frames.append(("production_candidate", production, args.production))
        if args.masked_bridge:
            frames.append(
                ("masked_bridge", masked_bridge_candidates(snapshot, snapshot_id, created_at), args.masked_bridge)
            )
        frames.append(
            ("hard_confuser", hard_confuser_candidates(snapshot, snapshot_id, created_at), args.hard_confuser)
        )
        frames.append(
            (
                "independent_miss",
                independent_miss_candidates(snapshot, snapshot_id, created_at),
                args.independent_miss,
            )
        )
        frames.append(("uniform", uniform_candidates(snapshot, snapshot_id, created_at, rng), args.uniform))

        excluded_pairs = excluded_entry_pairs(args.exclude_task_ledger)
        if excluded_pairs:
            frames = [
                (
                    frame,
                    [
                        task
                        for task in candidates
                        if tuple(sorted(task["hidden"]["current_entry_ids"])) not in excluded_pairs
                    ],
                    quota,
                )
                for frame, candidates, quota in frames
            ]

        baseline_pairs = source_pairs_from_db(args.baseline_db)
        playlist_pairs = source_pairs_from_db(args.playlist_db)
        for _, candidates, _ in frames:
            tag_candidate_origins(candidates, snapshot, baseline_pairs, playlist_pairs)

        selected: list[dict[str, Any]] = []
        selected_item_ids: set[str] = set()
        for _, candidates, quota in frames:
            frame_selected = stratified_sample(candidates, quota, rng, selected_item_ids)
            selected.extend(frame_selected)
            selected_item_ids.update(task["item_id"] for task in frame_selected)
        selected.sort(key=lambda task: (task["hidden"]["sampling_frame"], task["item_id"]))

        write_jsonl(args.private_out, selected)
        write_jsonl(args.blind_out, (blind_projection(task) for task in selected))

        counts = Counter(task["hidden"]["sampling_frame"] for task in selected)
        if args.manifest_out is not None:
            manifest = {
                "dataset_version": "dedup-entry-pilot/3",
                "schema_version": SCHEMA_VERSION,
                "ontology_version": ONTOLOGY_VERSION,
                "policy_version": POLICY_VERSION,
                "created_at": created_at,
                "seed": args.seed,
                "corpus": {
                    "path": str(args.db),
                    "sha256": snapshot_id,
                    "baseline_path": str(args.baseline_db) if args.baseline_db else None,
                    "baseline_sha256": (
                        sha256_file(args.baseline_db)
                        if args.baseline_db is not None and args.baseline_db.exists()
                        else None
                    ),
                    "playlist_path": str(args.playlist_db) if args.playlist_db else None,
                    "playlist_sha256": (
                        sha256_file(args.playlist_db)
                        if args.playlist_db is not None and args.playlist_db.exists()
                        else None
                    ),
                },
                "invariant": "every task compares two distinct complete musiclib entries",
                "task_count": len(selected),
                "frames": {
                    frame: {
                        "eligible": len(candidates),
                        "requested": quota,
                        "selected": counts[frame],
                    }
                    for frame, candidates, quota in frames
                },
                "entry_types": dict(sorted(Counter(task["entry_type"] for task in selected).items())),
                "script_relations": dict(
                    sorted(
                        Counter(coarse_stratum(task).split("/", 1)[1] for task in selected).items()
                    )
                ),
                "origin_pairs": dict(
                    sorted(Counter(task["hidden"]["stratum"].split("/")[-1] for task in selected).items())
                ),
                "excluded_task_ledgers": [
                    {"path": str(path), "sha256": sha256_file(path)}
                    for path in args.exclude_task_ledger
                    if path.exists()
                ],
            }
            args.manifest_out.parent.mkdir(parents=True, exist_ok=True)
            args.manifest_out.write_text(
                json.dumps(manifest, ensure_ascii=False, sort_keys=True, indent=2) + "\n",
                encoding="utf-8",
            )

        print(f"snapshot_id={snapshot_id}")
        print(f"private_out={args.private_out} blind_out={args.blind_out}")
        print(f"tasks={len(selected)}")
        for frame, candidates, quota in frames:
            print(f"{frame}: eligible={len(candidates)} requested={quota} selected={counts[frame]}")
        by_type = Counter(task["entry_type"] for task in selected)
        print("entry_types=" + canonical_json(dict(sorted(by_type.items()))))
        by_script = Counter(coarse_stratum(task).split("/", 1)[1] for task in selected)
        print("script_relations=" + canonical_json(dict(sorted(by_script.items()))))
    finally:
        snapshot.close()


if __name__ == "__main__":
    main()
