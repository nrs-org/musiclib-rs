from __future__ import annotations

import json
import math
import re
import unicodedata
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from difflib import SequenceMatcher
from pathlib import Path
from typing import Any, Iterable

from pykakasi import kakasi


DECISIVE = {"same_identity", "different_identity"}
VERSION_MARKERS = {
    "acoustic", "arrange", "arranged", "bootleg", "cover", "demo", "edit",
    "instrumental", "karaoke", "live", "mix", "remaster", "remastered", "remix",
    "reprise", "spedup", "version", "ver", "radio", "unplugged",
    "アコースティック", "アレンジ", "インスト", "カバー", "ライブ", "リミックス",
}


def load_jsonl(path: str | Path) -> list[dict[str, Any]]:
    with Path(path).open(encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def normalize(text: str) -> str:
    text = unicodedata.normalize("NFKC", text).casefold()
    chars = [ch if ch.isalnum() else " " for ch in text]
    return " ".join("".join(chars).split())


def compact(text: str) -> str:
    return normalize(text).replace(" ", "")


def tokens(text: str) -> set[str]:
    return {token for token in normalize(text).split() if len(token) > 1}


def ngrams(text: str, n: int = 3) -> set[str]:
    value = compact(text)
    if not value:
        return set()
    if len(value) <= n:
        return {value}
    return {value[index:index + n] for index in range(len(value) - n + 1)}


def jaccard(left: set[str], right: set[str]) -> float:
    union = left | right
    return len(left & right) / len(union) if union else 0.0


def best_similarity(left: Iterable[str], right: Iterable[str]) -> float:
    a = list(left)
    b = list(right)
    if not a or not b:
        return 0.0
    return max(SequenceMatcher(None, x, y).ratio() for x in a for y in b)


def qualifiers(text: str) -> set[str]:
    result = set()
    for content in re.findall(r"[\(\[【（]([^\)\]】）]+)[\)\]】）]", text):
        result.update(tokens(content))
    normalized = normalize(text)
    result.update(token for token in normalized.split() if token in VERSION_MARKERS)
    return result - {"ver", "version"}


def base_title(text: str) -> str:
    without_groups = re.sub(r"[\(\[【（][^\)\]】）]+[\)\]】）]", " ", text)
    return normalize(without_groups)


_KANA = {
    "あ":"a","い":"i","う":"u","え":"e","お":"o","か":"ka","き":"ki","く":"ku","け":"ke","こ":"ko",
    "さ":"sa","し":"shi","す":"su","せ":"se","そ":"so","た":"ta","ち":"chi","つ":"tsu","て":"te","と":"to",
    "な":"na","に":"ni","ぬ":"nu","ね":"ne","の":"no","は":"ha","ひ":"hi","ふ":"fu","へ":"he","ほ":"ho",
    "ま":"ma","み":"mi","む":"mu","め":"me","も":"mo","や":"ya","ゆ":"yu","よ":"yo","ら":"ra","り":"ri",
    "る":"ru","れ":"re","ろ":"ro","わ":"wa","を":"o","ん":"n","が":"ga","ぎ":"gi","ぐ":"gu","げ":"ge",
    "ご":"go","ざ":"za","じ":"ji","ず":"zu","ぜ":"ze","ぞ":"zo","だ":"da","ぢ":"ji","づ":"zu","で":"de",
    "ど":"do","ば":"ba","び":"bi","ぶ":"bu","べ":"be","ぼ":"bo","ぱ":"pa","ぴ":"pi","ぷ":"pu","ぺ":"pe","ぽ":"po",
    "きゃ":"kya","きゅ":"kyu","きょ":"kyo","しゃ":"sha","しゅ":"shu","しょ":"sho","ちゃ":"cha","ちゅ":"chu","ちょ":"cho",
    "にゃ":"nya","にゅ":"nyu","にょ":"nyo","ひゃ":"hya","ひゅ":"hyu","ひょ":"hyo","みゃ":"mya","みゅ":"myu","みょ":"myo",
    "りゃ":"rya","りゅ":"ryu","りょ":"ryo","ぎゃ":"gya","ぎゅ":"gyu","ぎょ":"gyo","じゃ":"ja","じゅ":"ju","じょ":"jo",
    "びゃ":"bya","びゅ":"byu","びょ":"byo","ぴゃ":"pya","ぴゅ":"pyu","ぴょ":"pyo",
}


def kana_to_romaji(text: str) -> str:
    hira = "".join(chr(ord(ch) - 0x60) if "ァ" <= ch <= "ヶ" else ch for ch in text)
    output: list[str] = []
    index = 0
    geminate = False
    while index < len(hira):
        if hira[index] == "っ":
            geminate = True
            index += 1
            continue
        pair = hira[index:index + 2]
        value = _KANA.get(pair)
        if value is not None:
            index += 2
        else:
            value = _KANA.get(hira[index], hira[index])
            index += 1
        if geminate and value and value[0].isascii() and value[0].isalpha():
            value = value[0] + value
        geminate = False
        if value == "ー" and output:
            vowel = next((ch for ch in reversed(output[-1]) if ch in "aeiou"), "")
            value = vowel
        output.append(value)
    return normalize("".join(output))


_KAKASI = kakasi()


def romanize_japanese(text: str) -> str:
    """Convert kana and kanji to normalized Hepburn while preserving Latin text."""
    converted = " ".join(part["hepburn"] for part in _KAKASI.convert(text))
    return normalize(converted)


def pair_key(left: str, right: str) -> tuple[str, str]:
    return (left, right) if left < right else (right, left)


@dataclass
class View:
    view_id: str
    entry_type: str
    title: str
    records: list[dict[str, Any]]
    names: set[str] = field(default_factory=set)
    romanized_names: set[str] = field(default_factory=set)
    artist_names: set[str] = field(default_factory=set)
    track_titles: list[str] = field(default_factory=list)
    dates: set[str] = field(default_factory=set)
    durations: set[int] = field(default_factory=set)
    identifiers: set[str] = field(default_factory=set)
    markers: set[str] = field(default_factory=set)
    qualifiers: set[str] = field(default_factory=set)
    base_names: set[str] = field(default_factory=set)
    parent_release_groups: set[str] = field(default_factory=set)
    parent_releases: set[str] = field(default_factory=set)
    credited_titles: set[str] = field(default_factory=set)
    primary_types: set[str] = field(default_factory=set)
    classifications: set[str] = field(default_factory=set)
    track_positions: set[tuple[int | None, int | None]] = field(default_factory=set)
    role_credits: dict[str, set[str]] = field(default_factory=lambda: defaultdict(set))
    primary_aliases_by_record: list[str] = field(default_factory=list)
    semantic_vector: list[float] | None = None

    @classmethod
    def from_packet(cls, packet: dict[str, Any]) -> "View":
        view = cls(
            view_id=packet["view_id"],
            entry_type=packet["entry_type"],
            title=packet.get("display_title") or "",
            records=packet["records"],
        )
        if view.title:
            view.names.add(normalize(view.title))
        for record in view.records:
            source = record["source"]
            identifier = str(record["identifier"])
            if source in {"barcode", "upc", "isrc"} or re.fullmatch(r"[A-Z]{2}[A-Z0-9]{3}\d{7}|\d{12,14}", identifier, re.I):
                view.identifiers.add(normalize(identifier))
            for fact in record["facts"]:
                field_name = fact["field"]
                value = fact["value"]
                if field_name == "alias" and isinstance(value, dict):
                    alias = normalize(str(value.get("name", "")))
                    view.names.add(alias)
                    if value.get("primary") and alias:
                        view.primary_aliases_by_record.append(alias)
                elif field_name == "artist_credit" and isinstance(value, dict):
                    credit = normalize(str(value.get("name", "")))
                    view.artist_names.add(credit)
                    view.role_credits[normalize(str(value.get("role", "unknown")))].add(credit)
                elif field_name == "tracklist_item" and isinstance(value, dict) and value.get("track_no") is not None:
                    view.track_titles.append(normalize(str(value.get("title", ""))))
                elif field_name == "credited_on" and isinstance(value, dict):
                    view.credited_titles.add(normalize(str(value.get("title", ""))))
                elif field_name == "parent_release_group" and isinstance(value, dict):
                    view.parent_release_groups.add(normalize(str(value.get("title", ""))))
                elif field_name == "parent_release" and isinstance(value, dict):
                    view.parent_releases.add(normalize(str(value.get("title", ""))))
                elif field_name == "parent_entity" and isinstance(value, dict):
                    if value.get("entry_type") == "artist":
                        view.artist_names.add(normalize(str(value.get("title", ""))))
                elif field_name == "track_position" and isinstance(value, dict):
                    view.track_positions.add((value.get("disc_no"), value.get("track_no")))
                elif field_name == "primary_type":
                    view.primary_types.add(normalize(str(value)))
                elif field_name == "source_classification" and isinstance(value, dict):
                    if value.get("vocabulary") != "discogs_release_status":
                        view.classifications.add(normalize(str(value.get("value", ""))))
                elif field_name == "release_date":
                    view.dates.add(str(value).split()[0])
                elif field_name == "duration_ms" and isinstance(value, int):
                    view.durations.add(value)
                elif field_name in {"barcode", "isrc"}:
                    view.identifiers.add(normalize(str(value)))
        view.names.discard("")
        view.artist_names.discard("")
        view.track_titles = [title for title in view.track_titles if title]
        for name in view.names:
            romanized = romanize_japanese(name)
            if romanized != name or name.isascii():
                view.romanized_names.add(romanized)
            view.markers.update(token for token in tokens(name) if token in VERSION_MARKERS)
            view.qualifiers.update(qualifiers(name))
            base = base_title(name)
            if base:
                view.base_names.add(base)
        return view

@dataclass
class LabeledPair:
    item_id: str
    left: str
    right: str
    entry_type: str
    frame: str
    stratum: str
    split_keys: list[str]
    label: str
    vote_a: str
    vote_b: str
    status: str


def _load_annotations(directory: str | Path) -> dict[str, dict[str, Any]]:
    result = {}
    source = Path(directory)
    paths = sorted(source.glob("*.jsonl")) if source.is_dir() else [source]
    for path in paths:
        for row in load_jsonl(path):
            result[row["item_id"]] = row
    return result


def load_dataset(
    task_path: str | Path,
    lane_a_path: str | Path,
    lane_b_path: str | Path,
    adjudication_path: str | Path,
    corpus_views_path: str | Path | None = None,
) -> tuple[dict[str, View], list[LabeledPair]]:
    tasks = load_jsonl(task_path)
    lane_a = _load_annotations(lane_a_path)
    lane_b = _load_annotations(lane_b_path)
    latest = {}
    for row in load_jsonl(adjudication_path):
        latest[row["item_id"]] = row
    views: dict[str, View] = {}
    entry_views: dict[int, str] = {}
    if corpus_views_path is not None:
        for row in load_jsonl(corpus_views_path):
            candidate = View.from_packet(row["view"])
            views[candidate.view_id] = candidate
            entry_views[int(row["entry_id"])] = candidate.view_id
    pairs = []
    for task in tasks:
        if corpus_views_path is None:
            for side in ("left", "right"):
                candidate = View.from_packet(task[side])
                previous = views.get(candidate.view_id)
                if previous is None:
                    views[candidate.view_id] = candidate
                elif previous.entry_type != candidate.entry_type:
                    raise ValueError(f"view type changed: {candidate.view_id}")
        item_id = task["item_id"]
        vote_a = lane_a[item_id]["judgment"]["factual"]["entity_relation"]
        vote_b = lane_b[item_id]["judgment"]["factual"]["entity_relation"]
        adjudication = latest.get(item_id)
        if adjudication is None:
            label = vote_a if vote_a == vote_b else "unresolved"
            status = "agreed" if vote_a == vote_b else "missing_adjudication"
        elif adjudication["status"] == "excluded":
            label = "excluded"
            status = "excluded"
        else:
            label = adjudication["judgment"]["factual"]["entity_relation"]
            status = adjudication["status"]
        hidden = task.get("hidden", {})
        if corpus_views_path is None:
            left_view = task["left"]["view_id"]
            right_view = task["right"]["view_id"]
        else:
            entry_ids = hidden.get("current_entry_ids", [])
            if len(entry_ids) != 2 or entry_ids[0] == entry_ids[1]:
                raise ValueError(f"task lacks two distinct corpus entry ids: {item_id}")
            try:
                left_view, right_view = (entry_views[int(entry_ids[0])], entry_views[int(entry_ids[1])])
            except KeyError as error:
                raise ValueError(f"task entry missing from corpus export: {item_id}") from error
        pairs.append(
            LabeledPair(
                item_id=item_id,
                left=left_view,
                right=right_view,
                entry_type=task["entry_type"],
                frame=hidden.get("sampling_frame", "unknown"),
                stratum=hidden.get("stratum", ""),
                split_keys=hidden.get("split_group_keys", []),
                label=label,
                vote_a=vote_a,
                vote_b=vote_b,
                status=status,
            )
        )
    return views, pairs


def _emit_blocks(
    postings: dict[tuple[str, str], list[str]],
    reason: str,
    candidates: dict[tuple[str, str], set[str]],
    max_block: int,
) -> None:
    for members in postings.values():
        unique = sorted(set(members))
        if len(unique) < 2 or len(unique) > max_block:
            continue
        for i, left in enumerate(unique):
            for right in unique[i + 1:]:
                candidates[pair_key(left, right)].add(reason)


def generate_candidates(
    views: dict[str, View],
    *,
    max_block: int = 50,
    ngram_k: int = 30,
    semantic: dict[str, list[float]] | None = None,
    semantic_k: int = 20,
    semantic_threshold: float = 0.45,
    candidate_k: int = 50,
) -> dict[tuple[str, str], set[str]]:
    candidates: dict[tuple[str, str], set[str]] = defaultdict(set)
    exact: dict[tuple[str, str], list[str]] = defaultdict(list)
    romanized: dict[tuple[str, str], list[str]] = defaultdict(list)
    identifiers: dict[tuple[str, str], list[str]] = defaultdict(list)
    tracklists: dict[tuple[str, str], list[str]] = defaultdict(list)
    base_titles: dict[tuple[str, str], list[str]] = defaultdict(list)
    duration_credits: dict[tuple[str, str, int], list[str]] = defaultdict(list)
    gram_postings: dict[tuple[str, str], list[str]] = defaultdict(list)
    token_postings: dict[tuple[str, str], list[str]] = defaultdict(list)
    for view in views.values():
        for name in view.names:
            if len(compact(name)) >= 2:
                exact[(view.entry_type, compact(name))].append(view.view_id)
            for token in tokens(name):
                token_postings[(view.entry_type, token)].append(view.view_id)
            for gram in ngrams(name):
                gram_postings[(view.entry_type, gram)].append(view.view_id)
        for name in view.romanized_names:
            if len(compact(name)) >= 3:
                romanized[(view.entry_type, compact(name))].append(view.view_id)
        for identifier in view.identifiers:
            identifiers[(view.entry_type, identifier)].append(view.view_id)
        if view.track_titles:
            fingerprint = "|".join(view.track_titles)
            tracklists[(view.entry_type, fingerprint)].append(view.view_id)
        for title in view.base_names:
            if len(compact(title)) >= 3:
                base_titles[(view.entry_type, compact(title))].append(view.view_id)
        if view.entry_type == "track":
            for artist in view.artist_names:
                for duration in view.durations:
                    bucket = duration // 5000
                    for nearby in (bucket - 1, bucket, bucket + 1):
                        duration_credits[(view.entry_type, compact(artist), nearby)].append(view.view_id)

    _emit_blocks(exact, "exact_name", candidates, max_block)
    _emit_blocks(romanized, "romanized_name", candidates, max_block)
    _emit_blocks(identifiers, "identifier", candidates, max_block)
    _emit_blocks(tracklists, "tracklist", candidates, max_block)
    _emit_blocks(base_titles, "base_title", candidates, max_block)
    _emit_blocks(duration_credits, "duration_credit", candidates, max_block)
    _emit_blocks(token_postings, "token", candidates, max_block)

    overlaps: dict[str, Counter[str]] = defaultdict(Counter)
    for members in gram_postings.values():
        unique = sorted(set(members))
        if len(unique) < 2 or len(unique) > max_block:
            continue
        for left in unique:
            for right in unique:
                if left < right:
                    overlaps[left][right] += 1
                    overlaps[right][left] += 1
    for left, counts in overlaps.items():
        for right, shared in counts.most_common(ngram_k):
            if shared >= 2:
                candidates[pair_key(left, right)].add("char_ngram")

    release_overlap: dict[str, Counter[str]] = defaultdict(Counter)
    track_postings: dict[str, list[str]] = defaultdict(list)
    for view in views.values():
        if view.entry_type == "release":
            for title in set(view.track_titles):
                track_postings[title].append(view.view_id)
    for members in track_postings.values():
        unique = sorted(set(members))
        if not 2 <= len(unique) <= max_block:
            continue
        for index, left in enumerate(unique):
            for right in unique[index + 1:]:
                release_overlap[left][right] += 1
                release_overlap[right][left] += 1
    for left, counts in release_overlap.items():
        left_titles = set(views[left].track_titles)
        for right, shared in counts.most_common(ngram_k):
            right_titles = set(views[right].track_titles)
            union = left_titles | right_titles
            if shared >= 2 and union and shared / len(union) >= 0.18:
                candidates[pair_key(left, right)].add("tracklist_overlap")

    if semantic:
        try:
            import hnswlib
            import numpy as np
        except ImportError as error:
            raise RuntimeError(
                "semantic ANN dependencies are missing; run with `uv run --extra semantic`"
            ) from error
        by_type: dict[str, list[str]] = defaultdict(list)
        for view in views.values():
            if view.view_id in semantic:
                by_type[view.entry_type].append(view.view_id)
        for members in by_type.values():
            members.sort()
            if len(members) < 2:
                continue
            matrix = np.asarray([semantic[view_id] for view_id in members], dtype=np.float32)
            index = hnswlib.Index(space="cosine", dim=matrix.shape[1])
            index.init_index(
                max_elements=len(members), ef_construction=160, M=24, random_seed=20260914
            )
            index.add_items(matrix, np.arange(len(members)), num_threads=1)
            index.set_ef(max(50, semantic_k * 3))
            labels, distances = index.knn_query(
                matrix, k=min(len(members), semantic_k + 1), num_threads=1
            )
            for left_index, (neighbors, neighbor_distances) in enumerate(zip(labels, distances)):
                left = members[left_index]
                for right_index, distance in zip(neighbors, neighbor_distances):
                    if int(right_index) == left_index:
                        continue
                    similarity = 1.0 - float(distance)
                    if similarity >= semantic_threshold:
                        candidates[pair_key(left, members[int(right_index)])].add("semantic_ann")

    # Every individual channel is already bounded by posting size or per-entry
    # nearest-neighbour count. A zero global cap preserves that channel-level
    # recall; a positive value additionally constrains the union.
    if candidate_k == 0:
        return dict(candidates)
    if candidate_k < 0:
        raise ValueError("candidate_k must be non-negative")
    priorities = {
        "identifier": 10,
        "exact_name": 9,
        "romanized_name": 8,
        "tracklist": 8,
        "duration_credit": 7,
        "tracklist_overlap": 7,
        "base_title": 6,
        "semantic_ann": 5,
        "token": 4,
        "char_ngram": 3,
    }
    neighbors: dict[str, list[tuple[tuple[float, ...], tuple[str, str]]]] = defaultdict(list)
    for key, reasons in candidates.items():
        left, right = views[key[0]], views[key[1]]
        values = features(left, right)
        rank = (
            float(max(priorities.get(reason, 0) for reason in reasons)),
            values["name_exact"],
            values["romanized_exact"],
            values["identifier_overlap"],
            values["artist_jaccard"],
            values["duration_similarity"],
            values["tracklist_jaccard"],
            values["semantic_similarity"],
            values["name_similarity"],
        )
        neighbors[key[0]].append((rank, key))
        neighbors[key[1]].append((rank, key))
    kept: set[tuple[str, str]] = set()
    for ranked in neighbors.values():
        ranked.sort(key=lambda item: (item[0], item[1]), reverse=True)
        kept.update(key for _, key in ranked[:candidate_k])
    return {key: candidates[key] for key in kept}


FEATURE_NAMES = [
    "name_exact", "name_similarity", "token_jaccard", "ngram_jaccard",
    "romanized_exact", "romanized_similarity", "identifier_overlap",
    "artist_jaccard", "tracklist_jaccard", "tracklist_ordered",
    "tracklist_length_similarity", "date_exact", "duration_similarity",
    "version_conflict", "base_title_exact", "qualifier_jaccard",
    "qualifier_conflict", "parent_group_jaccard", "parent_release_jaccard",
    "credited_title_jaccard", "primary_type_match", "primary_type_conflict",
    "classification_match", "track_position_match", "role_credit_jaccard",
    "role_credit_conflict", "internal_mixedness", "empty_side",
    "semantic_similarity",
]

# Exact subset reproducible from musiclib-rs EntryInfo. Keep the richer model
# for research, but never ask the runtime to invent unavailable enrichment.
RUNTIME_FEATURE_NAMES = [
    "name_exact", "name_similarity", "token_jaccard", "ngram_jaccard",
    "identifier_overlap", "artist_jaccard", "tracklist_jaccard",
    "tracklist_length_similarity", "date_exact", "duration_similarity",
    "version_conflict", "base_title_exact", "qualifier_jaccard",
    "qualifier_conflict", "primary_type_match", "primary_type_conflict",
    "track_position_match", "internal_mixedness", "empty_side",
]

NEGATIVE_FEATURES = {
    "version_conflict",
    "qualifier_conflict",
    "primary_type_conflict",
    "role_credit_conflict",
    "internal_mixedness",
    "empty_side",
}


def features(left: View, right: View) -> dict[str, float]:
    name_exact = bool({compact(name) for name in left.names} & {compact(name) for name in right.names})
    romanized_exact = bool(
        {compact(name) for name in left.romanized_names}
        & {compact(name) for name in right.romanized_names}
    )
    all_left_tokens = set().union(*(tokens(name) for name in left.names)) if left.names else set()
    all_right_tokens = set().union(*(tokens(name) for name in right.names)) if right.names else set()
    all_left_grams = set().union(*(ngrams(name) for name in left.names)) if left.names else set()
    all_right_grams = set().union(*(ngrams(name) for name in right.names)) if right.names else set()
    duration_similarity = 0.0
    if left.durations and right.durations:
        delta = min(abs(a - b) for a in left.durations for b in right.durations)
        duration_similarity = max(0.0, 1.0 - delta / 30_000)
    shared_roles = set(left.role_credits) & set(right.role_credits)
    role_overlap = (
        sum(jaccard(left.role_credits[role], right.role_credits[role]) for role in shared_roles)
        / len(shared_roles)
        if shared_roles
        else 0.0
    )
    identity_roles = {"arranger", "arranged by", "composer", "composed by", "performer"}
    role_conflict = any(
        left.role_credits[role]
        and right.role_credits[role]
        and not (left.role_credits[role] & right.role_credits[role])
        for role in shared_roles & identity_roles
    )
    tracklist_length_similarity = 0.0
    if left.track_titles and right.track_titles:
        tracklist_length_similarity = min(len(left.track_titles), len(right.track_titles)) / max(
            len(left.track_titles), len(right.track_titles)
        )
    internally_mixed = 0.0
    for view in (left, right):
        aliases = view.primary_aliases_by_record
        if len(aliases) >= 2:
            minimum = min(
                SequenceMatcher(None, a, b).ratio()
                for index, a in enumerate(aliases)
                for b in aliases[index + 1:]
            )
            internally_mixed = max(internally_mixed, 1.0 - minimum)
    base_exact = bool(left.base_names & right.base_names)
    qualifier_conflict = bool(
        base_exact
        and left.qualifiers != right.qualifiers
        and (left.qualifiers or right.qualifiers)
    )
    semantic_similarity = 0.0
    if left.semantic_vector is not None and right.semantic_vector is not None:
        a, b = left.semantic_vector, right.semantic_vector
        denom = math.sqrt(sum(x * x for x in a) * sum(x * x for x in b))
        semantic_similarity = sum(x * y for x, y in zip(a, b, strict=True)) / denom if denom else 0.0
    return {
        "name_exact": float(name_exact),
        "name_similarity": best_similarity(left.names, right.names),
        "token_jaccard": jaccard(all_left_tokens, all_right_tokens),
        "ngram_jaccard": jaccard(all_left_grams, all_right_grams),
        "romanized_exact": float(romanized_exact),
        "romanized_similarity": best_similarity(left.romanized_names, right.romanized_names),
        "identifier_overlap": float(bool(left.identifiers & right.identifiers)),
        "artist_jaccard": jaccard(left.artist_names, right.artist_names),
        "tracklist_jaccard": jaccard(set(left.track_titles), set(right.track_titles)),
        "tracklist_ordered": SequenceMatcher(None, left.track_titles, right.track_titles).ratio() if left.track_titles and right.track_titles else 0.0,
        "tracklist_length_similarity": tracklist_length_similarity,
        "date_exact": float(bool(left.dates & right.dates)),
        "duration_similarity": duration_similarity,
        "version_conflict": float(bool(left.markers ^ right.markers) and bool(left.markers | right.markers)),
        "base_title_exact": float(base_exact),
        "qualifier_jaccard": jaccard(left.qualifiers, right.qualifiers),
        "qualifier_conflict": float(qualifier_conflict),
        "parent_group_jaccard": jaccard(left.parent_release_groups, right.parent_release_groups),
        "parent_release_jaccard": jaccard(left.parent_releases, right.parent_releases),
        "credited_title_jaccard": jaccard(left.credited_titles, right.credited_titles),
        "primary_type_match": float(bool(left.primary_types & right.primary_types)),
        "primary_type_conflict": float(bool(left.primary_types and right.primary_types and not (left.primary_types & right.primary_types))),
        "classification_match": float(bool(left.classifications & right.classifications)),
        "track_position_match": float(bool(left.track_positions & right.track_positions)),
        "role_credit_jaccard": role_overlap,
        "role_credit_conflict": float(role_conflict),
        "internal_mixedness": internally_mixed,
        "empty_side": float(not left.names or not right.names),
        "semantic_similarity": semantic_similarity,
    }


def vectorize(values: dict[str, float], feature_names: list[str] = FEATURE_NAMES) -> list[float]:
    return [values[name] for name in feature_names]


class LogisticModel:
    def __init__(self, feature_names: list[str] = FEATURE_NAMES) -> None:
        self.feature_names = list(feature_names)
        self.means: list[float] = []
        self.scales: list[float] = []
        self.weights: list[float] = []
        self.bias = 0.0

    def fit(self, rows: list[list[float]], labels: list[int], epochs: int = 900, l2: float = 0.08) -> None:
        width = len(rows[0])
        self.means = [sum(row[j] for row in rows) / len(rows) for j in range(width)]
        self.scales = []
        for j in range(width):
            variance = sum((row[j] - self.means[j]) ** 2 for row in rows) / len(rows)
            self.scales.append(max(math.sqrt(variance), 1e-6))
        x = [self._standardize(row) for row in rows]
        self.weights = [0.0] * width
        prevalence = min(max(sum(labels) / len(labels), 1e-4), 1 - 1e-4)
        self.bias = math.log(prevalence / (1 - prevalence))
        for epoch in range(epochs):
            grad = [0.0] * width
            grad_bias = 0.0
            for row, label in zip(x, labels, strict=True):
                probability = self._sigmoid(self.bias + sum(w * value for w, value in zip(self.weights, row, strict=True)))
                error = probability - label
                grad_bias += error
                for j, value in enumerate(row):
                    grad[j] += error * value
            rate = 0.12 / (1 + epoch / 250)
            self.bias -= rate * grad_bias / len(rows)
            for j in range(width):
                self.weights[j] -= rate * (grad[j] / len(rows) + l2 * self.weights[j])
                # Sampling-frame bias must not make corroborating evidence such
                # as an identical tracklist count against identity.
                if self.feature_names[j] in NEGATIVE_FEATURES:
                    self.weights[j] = min(0.0, self.weights[j])
                else:
                    self.weights[j] = max(0.0, self.weights[j])

    @staticmethod
    def _sigmoid(value: float) -> float:
        value = max(-35.0, min(35.0, value))
        return 1.0 / (1.0 + math.exp(-value))

    def _standardize(self, row: list[float]) -> list[float]:
        return [(value - mean) / scale for value, mean, scale in zip(row, self.means, self.scales, strict=True)]

    def predict(self, row: list[float]) -> float:
        values = self._standardize(row)
        return self._sigmoid(self.bias + sum(w * value for w, value in zip(self.weights, values, strict=True)))

    def coefficients(self) -> dict[str, float]:
        return {name: weight / scale for name, weight, scale in zip(self.feature_names, self.weights, self.scales, strict=True)}

    def export(self) -> dict[str, Any]:
        """Return an inference-only model in the original feature space.

        Training standardizes every feature, but consumers should not need to
        reproduce that implementation detail. Folding the means/scales into a
        raw coefficient vector and intercept makes the artifact both portable
        and easy to verify in another language.
        """
        coefficients = self.coefficients()
        intercept = self.bias - sum(
            coefficients[name] * mean
            for name, mean in zip(self.feature_names, self.means, strict=True)
        )
        return {
            "intercept": intercept,
            "coefficients": coefficients,
        }


class DSU:
    def __init__(self, size: int):
        self.parent = list(range(size))

    def find(self, value: int) -> int:
        while self.parent[value] != value:
            self.parent[value] = self.parent[self.parent[value]]
            value = self.parent[value]
        return value

    def union(self, left: int, right: int) -> None:
        a, b = self.find(left), self.find(right)
        if a != b:
            self.parent[b] = a


def grouped_folds(pairs: list[LabeledPair], folds: int = 5) -> list[int]:
    dsu = DSU(len(pairs))
    owners: dict[str, int] = {}
    for index, pair in enumerate(pairs):
        keys = [f"view:{pair.left}", f"view:{pair.right}", *(f"split:{key}" for key in pair.split_keys)]
        for key in keys:
            if key in owners:
                dsu.union(index, owners[key])
            else:
                owners[key] = index
    components: dict[int, list[int]] = defaultdict(list)
    for index in range(len(pairs)):
        components[dsu.find(index)].append(index)
    assignments = [-1] * len(pairs)
    fold_sizes = [0] * folds
    fold_positives = [0] * folds
    ordered = sorted(components.values(), key=lambda members: (-len(members), pairs[members[0]].item_id))
    for members in ordered:
        positives = sum(pairs[index].label == "same_identity" for index in members)
        target = min(range(folds), key=lambda fold: (fold_sizes[fold], fold_positives[fold], fold))
        for index in members:
            assignments[index] = target
        fold_sizes[target] += len(members)
        fold_positives[target] += positives
    return assignments


def cross_validated_probabilities(
    pairs: list[LabeledPair], views: dict[str, View], folds: int = 5,
    feature_names: list[str] = FEATURE_NAMES,
) -> tuple[dict[str, float], dict[str, LogisticModel]]:
    decisive = [pair for pair in pairs if pair.label in DECISIVE]
    fold_ids = grouped_folds(decisive, folds)
    rows = [vectorize(features(views[pair.left], views[pair.right]), feature_names) for pair in decisive]
    labels = [int(pair.label == "same_identity") for pair in decisive]
    probabilities: dict[str, float] = {}
    for fold in range(folds):
        for entry_type in sorted({pair.entry_type for pair in decisive}):
            train = [
                index
                for index, assigned in enumerate(fold_ids)
                if assigned != fold and decisive[index].entry_type == entry_type
            ]
            test = [
                index
                for index, assigned in enumerate(fold_ids)
                if assigned == fold and decisive[index].entry_type == entry_type
            ]
            if not test:
                continue
            model = LogisticModel(feature_names)
            model.fit([rows[index] for index in train], [labels[index] for index in train])
            for index in test:
                probabilities[decisive[index].item_id] = model.predict(rows[index])
    final: dict[str, LogisticModel] = {}
    for entry_type in sorted({pair.entry_type for pair in decisive}):
        selected = [index for index, pair in enumerate(decisive) if pair.entry_type == entry_type]
        model = LogisticModel(feature_names)
        model.fit([rows[index] for index in selected], [labels[index] for index in selected])
        final[entry_type] = model
    return probabilities, final


def conservative_thresholds(pairs: list[LabeledPair], probabilities: dict[str, float]) -> tuple[float, float]:
    positives = [probabilities[pair.item_id] for pair in pairs if pair.label == "same_identity"]
    negatives = [probabilities[pair.item_id] for pair in pairs if pair.label == "different_identity"]
    merge = min(1.0, max(negatives) + 1e-9)
    separate = max(0.0, min(positives) - 1e-9)
    return merge, separate


def precision_thresholds(
    pairs: list[LabeledPair], probabilities: dict[str, float], target: float
) -> tuple[float, float]:
    """Maximize decided coverage while meeting precision on each decision arm."""
    if not 0.5 < target <= 1.0:
        raise ValueError("target precision must be in (0.5, 1.0]")
    ranked = [
        (probabilities[pair.item_id], pair.label == "same_identity")
        for pair in pairs if pair.label in DECISIVE
    ]
    thresholds = sorted({score for score, _ in ranked})
    merge_options = []
    separate_options = []
    for threshold in thresholds:
        merges = [label for score, label in ranked if score >= threshold]
        separates = [label for score, label in ranked if score <= threshold]
        if merges and sum(merges) / len(merges) >= target:
            merge_options.append((len(merges), -threshold, threshold))
        if separates and sum(not label for label in separates) / len(separates) >= target:
            separate_options.append((len(separates), threshold))
    if not merge_options or not separate_options:
        return conservative_thresholds(pairs, probabilities)
    merge = max(merge_options)[2]
    separate = max(separate_options)[1]
    if separate >= merge:
        return conservative_thresholds(pairs, probabilities)
    return merge, separate


def metric_counts(pairs: list[LabeledPair], probabilities: dict[str, float], merge: float, separate: float) -> dict[str, Any]:
    confusion = Counter()
    for pair in pairs:
        if pair.label not in DECISIVE:
            continue
        probability = probabilities[pair.item_id]
        predicted = "same_identity" if probability >= merge else "different_identity" if probability <= separate else "defer"
        confusion[(pair.label, predicted)] += 1
    same_predicted = sum(count for (gold, pred), count in confusion.items() if pred == "same_identity")
    same_correct = confusion[("same_identity", "same_identity")]
    separate_predicted = sum(count for (_, pred), count in confusion.items() if pred == "different_identity")
    separate_correct = confusion[("different_identity", "different_identity")]
    decided = same_predicted + separate_predicted
    fixed = Counter()
    ranked = []
    for pair in pairs:
        if pair.label not in DECISIVE:
            continue
        probability = probabilities[pair.item_id]
        fixed[(pair.label, "same_identity" if probability >= 0.5 else "different_identity")] += 1
        ranked.append((probability, int(pair.label == "same_identity")))
    fixed_same_predictions = sum(
        count for (_, predicted), count in fixed.items() if predicted == "same_identity"
    )
    fixed_true_positives = fixed[("same_identity", "same_identity")]
    positives = sum(label for _, label in ranked)
    negatives = len(ranked) - positives
    wins = 0.0
    for positive_score, label in ranked:
        if not label:
            continue
        for negative_score, other_label in ranked:
            if other_label:
                continue
            wins += positive_score > negative_score
            wins += 0.5 * (positive_score == negative_score)
    return {
        "merge_threshold": merge,
        "separate_threshold": separate,
        "auto_merge_precision": same_correct / same_predicted if same_predicted else None,
        "auto_merge_recall": same_correct / sum(count for (gold, _), count in confusion.items() if gold == "same_identity"),
        "auto_separate_precision": separate_correct / separate_predicted if separate_predicted else None,
        "decided_accuracy": (same_correct + separate_correct) / decided if decided else None,
        "coverage": sum(count for (_, pred), count in confusion.items() if pred != "defer") / sum(confusion.values()),
        "confusion": {f"{gold}->{pred}": count for (gold, pred), count in sorted(confusion.items())},
        "roc_auc": wins / (positives * negatives) if positives and negatives else None,
        "fixed_0_5": {
            "merge_precision": fixed_true_positives / fixed_same_predictions if fixed_same_predictions else None,
            "merge_recall": fixed_true_positives / positives if positives else None,
            "accuracy": sum(count for (gold, predicted), count in fixed.items() if gold == predicted) / len(ranked),
            "confusion": {f"{gold}->{predicted}": count for (gold, predicted), count in sorted(fixed.items())},
        },
    }


def evaluate(
    views: dict[str, View],
    pairs: list[LabeledPair],
    candidates: dict[tuple[str, str], set[str]],
    probabilities: dict[str, float],
    final_models: dict[str, LogisticModel],
    merge_threshold: float | None = None,
    separate_threshold: float | None = None,
    target_precision: float | None = None,
) -> dict[str, Any]:
    if target_precision is None:
        default_merge, default_separate = conservative_thresholds(pairs, probabilities)
    else:
        default_merge, default_separate = precision_thresholds(pairs, probabilities, target_precision)
    merge = default_merge if merge_threshold is None else merge_threshold
    separate = default_separate if separate_threshold is None else separate_threshold
    if separate >= merge:
        raise ValueError("separate threshold must be lower than merge threshold")
    metrics = metric_counts(pairs, probabilities, merge, separate)
    same_pairs = [pair for pair in pairs if pair.label == "same_identity"]
    retrieved = [pair for pair in same_pairs if pair_key(pair.left, pair.right) in candidates]
    possible = 0
    by_type = Counter(view.entry_type for view in views.values())
    for count in by_type.values():
        possible += count * (count - 1) // 2
    channel_hits = Counter()
    exclusive_hits = Counter()
    channel_candidates = Counter()
    degree = Counter()
    for (left, right), reasons in candidates.items():
        degree[left] += 1
        degree[right] += 1
        channel_candidates.update(reasons)
    results = []
    for pair in pairs:
        key = pair_key(pair.left, pair.right)
        reasons = sorted(candidates.get(key, set()))
        for reason in reasons:
            if pair.label == "same_identity":
                channel_hits[reason] += 1
        if pair.label == "same_identity" and len(reasons) == 1:
            exclusive_hits[reasons[0]] += 1
        row_features = features(views[pair.left], views[pair.right])
        probability = probabilities.get(pair.item_id)
        prediction = None
        if probability is not None:
            prediction = "same_identity" if probability >= merge else "different_identity" if probability <= separate else "defer"
        results.append(
            {
                "item_id": pair.item_id,
                "short_id": pair.item_id.removeprefix("sha256:")[:12],
                "type": pair.entry_type,
                "frame": pair.frame,
                "stratum": pair.stratum,
                "left_title": views[pair.left].title,
                "right_title": views[pair.right].title,
                "gold": pair.label,
                "status": pair.status,
                "vote_a": pair.vote_a,
                "vote_b": pair.vote_b,
                "retrieved": bool(reasons),
                "channels": reasons,
                "probability_same": probability,
                "probability_kind": "grouped_oof" if probability is not None else None,
                "prediction": prediction,
                "features": row_features,
            }
        )
    labeled_by_key = {pair_key(pair.left, pair.right): pair for pair in pairs}
    candidate_results = []
    for key, reasons in candidates.items():
        left, right = (views[key[0]], views[key[1]])
        row_features = features(left, right)
        model = final_models[left.entry_type]
        probability = model.predict(vectorize(row_features, model.feature_names))
        prediction = (
            "same_identity"
            if probability >= merge
            else "different_identity"
            if probability <= separate
            else "defer"
        )
        known = labeled_by_key.get(key)
        candidate_results.append(
            {
                "item_id": known.item_id if known else None,
                "short_id": (
                    known.item_id.removeprefix("sha256:")[:12]
                    if known
                    else f"{key[0].removeprefix('sha256:')[:6]}-{key[1].removeprefix('sha256:')[:6]}"
                ),
                "type": left.entry_type,
                "frame": known.frame if known else "generated",
                "stratum": known.stratum if known else "unlabeled candidate",
                "left_view_id": left.view_id,
                "right_view_id": right.view_id,
                "left_title": left.title,
                "right_title": right.title,
                "gold": known.label if known else "unlabeled",
                "status": known.status if known else "unlabeled",
                "vote_a": known.vote_a if known else None,
                "vote_b": known.vote_b if known else None,
                "retrieved": True,
                "channels": sorted(reasons),
                "probability_same": probability,
                "probability_kind": "full_fit",
                "prediction": prediction,
                "features": row_features,
            }
        )
    candidate_results.sort(key=lambda row: (-row["probability_same"], row["short_id"]))
    def retrieval_slices(attribute: str) -> dict[str, dict[str, int | float]]:
        result = {}
        for value in sorted({getattr(pair, attribute) for pair in pairs}):
            positives = [pair for pair in same_pairs if getattr(pair, attribute) == value]
            hits = sum(pair_key(pair.left, pair.right) in candidates for pair in positives)
            result[value] = {
                "retrieved": hits,
                "total": len(positives),
                "recall": hits / len(positives) if positives else 0.0,
            }
        return result

    empty_views = {
        view.view_id for view in views.values() if not view.names and not view.identifiers
    }
    missed_with_empty_view = sum(
        pair_key(pair.left, pair.right) not in candidates
        and (pair.left in empty_views or pair.right in empty_views)
        for pair in same_pairs
    )

    sorted_degrees = sorted(degree.get(view_id, 0) for view_id in views)

    def percentile(values: list[int], fraction: float) -> int:
        if not values:
            return 0
        return values[round((len(values) - 1) * fraction)]

    return {
        "dataset": {
            "tasks": len(pairs),
            "unique_views": len(views),
            "labels": dict(Counter(pair.label for pair in pairs)),
            "types": dict(Counter(pair.entry_type for pair in pairs)),
            "frames": dict(Counter(pair.frame for pair in pairs)),
        },
        "retrieval": {
            "candidate_pairs": len(candidates),
            "possible_same_type_pairs": possible,
            "candidate_fraction": len(candidates) / possible if possible else 0.0,
            "positive_recall": len(retrieved) / len(same_pairs) if same_pairs else 0.0,
            "positives_retrieved": len(retrieved),
            "positives_total": len(same_pairs),
            "channel_positive_hits": dict(channel_hits),
            "channel_exclusive_hits": dict(exclusive_hits),
            "channel_candidate_pairs": dict(channel_candidates),
            "candidate_degree": {
                "mean": sum(sorted_degrees) / len(sorted_degrees) if sorted_degrees else 0.0,
                "p50": percentile(sorted_degrees, 0.50),
                "p95": percentile(sorted_degrees, 0.95),
                "p99": percentile(sorted_degrees, 0.99),
                "max": max(sorted_degrees, default=0),
                "zero": sum(value == 0 for value in sorted_degrees),
            },
            "by_type": retrieval_slices("entry_type"),
            "by_frame": retrieval_slices("frame"),
            "empty_or_identifier_only_views": len(empty_views),
            "missed_positives_with_empty_view": missed_with_empty_view,
        },
        "scoring": {
            **metrics,
            "threshold_source": (
                f"target_{target_precision:.3f}_precision_oof"
                if target_precision is not None and merge_threshold is None and separate_threshold is None
                else "zero_observed_error_oof"
                if target_precision is None and merge_threshold is None and separate_threshold is None
                else "command_line_override"
            ),
            "coefficients_by_type": {
                entry_type: model.coefficients() for entry_type, model in final_models.items()
            },
            "model": {
                "schema": "musiclib-dedup-logistic/1",
                "features": next(iter(final_models.values())).feature_names,
                "models_by_type": {
                    entry_type: model.export() for entry_type, model in final_models.items()
                },
                "merge_threshold": merge,
                "separate_threshold": separate,
                "training": {
                    "algorithm": "sign-constrained-logistic-regression",
                    "epochs": 900,
                    "l2": 0.08,
                    "fit": "all decisive labeled rows, separated by entry type",
                },
            },
            "warning": "OOF results on a diagnostic, non-representative set. Thresholds are selected on these same OOF predictions and are not a deployment safety estimate.",
        },
        "items": results,
        "candidates": candidate_results,
    }
