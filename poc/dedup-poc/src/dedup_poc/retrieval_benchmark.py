from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any

from .core import View, generate_candidates, load_jsonl, pair_key
from .embeddings import DEFAULT_MODEL, embed_views


def sliced(probes: list[dict[str, Any]], hits: set[str], key: str) -> dict[str, Any]:
    output = {}
    for value in sorted({str(probe[key]) for probe in probes}):
        selected = [probe for probe in probes if str(probe[key]) == value]
        count = sum(probe["probe_id"] in hits for probe in selected)
        output[value] = {"retrieved": count, "total": len(selected), "recall": count / len(selected)}
    return output


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description="Counterfactual dedup retrieval benchmark")
    result.add_argument("--corpus-views", default="data/dedup-entry-pilot-v3.corpus-views.jsonl")
    result.add_argument("--probes", default="data/dedup-entry-retrieval-probes-v4.jsonl")
    result.add_argument("--output", default="data/dedup-entry-retrieval-probes-v4.report.json")
    result.add_argument("--no-semantic", action="store_true")
    result.add_argument("--embedding-model", default=DEFAULT_MODEL)
    result.add_argument("--allow-model-download", action="store_true")
    result.add_argument("--embedding-batch-size", type=int, default=32)
    result.add_argument("--max-block", type=int, default=50)
    result.add_argument("--ngram-k", type=int, default=30)
    result.add_argument(
        "--candidate-k", type=int, default=0,
        help="optional global per-entry union cap; 0 keeps channel-level bounds",
    )
    result.add_argument("--semantic-k", type=int, default=20)
    result.add_argument("--semantic-threshold", type=float, default=0.45)
    return result


def main() -> None:
    args = parser().parse_args()
    probes = load_jsonl(args.probes)
    replaced_ids = {int(probe["original_entry_id"]) for probe in probes}
    views: dict[str, View] = {}
    snapshot_ids = set()
    for row in load_jsonl(args.corpus_views):
        snapshot_ids.add(row["snapshot_id"])
        if int(row["entry_id"]) not in replaced_ids:
            view = View.from_packet(row["view"])
            views[view.view_id] = view
    for probe in probes:
        snapshot_ids.add(probe["snapshot_id"])
        for side in ("left", "right"):
            view = View.from_packet(probe[side])
            views[view.view_id] = view
    if len(snapshot_ids) != 1:
        raise ValueError("corpus and probes do not share one snapshot")

    semantic = None if args.no_semantic else embed_views(
        views,
        model_name=args.embedding_model,
        batch_size=args.embedding_batch_size,
        allow_download=args.allow_model_download,
    )
    candidates = generate_candidates(
        views,
        max_block=args.max_block,
        ngram_k=args.ngram_k,
        semantic=semantic,
        semantic_k=args.semantic_k,
        semantic_threshold=args.semantic_threshold,
        candidate_k=args.candidate_k,
    )
    hits = set()
    channel_hits: Counter[str] = Counter()
    exclusive_hits: Counter[str] = Counter()
    missed = []
    for probe in probes:
        key = pair_key(probe["left"]["view_id"], probe["right"]["view_id"])
        reasons = candidates.get(key, set())
        if reasons:
            hits.add(probe["probe_id"])
            channel_hits.update(reasons)
            if len(reasons) == 1:
                exclusive_hits.update(reasons)
        else:
            missed.append(
                {
                    key: probe[key]
                    for key in (
                        "probe_id", "entry_type", "script_relation", "exact_alias_overlap",
                        "best_name_similarity", "partition",
                    )
                }
            )
    nonexact = [probe for probe in probes if not probe["exact_alias_overlap"]]
    nonexact_hits = sum(probe["probe_id"] in hits for probe in nonexact)
    degree = Counter()
    for left, right in candidates:
        degree[left] += 1
        degree[right] += 1
    report = {
        "benchmark": "counterfactual_positive_retrieval",
        "warning": "Partitions of robustly linked source clusters; useful for retrieval recall, not a prevalence estimate.",
        "snapshot_id": next(iter(snapshot_ids)),
        "views": len(views),
        "candidate_pairs": len(candidates),
        "candidate_degree_mean": 2 * len(candidates) / len(views),
        "candidate_degree_max": max(degree.values(), default=0),
        "probes": len(probes),
        "retrieved": len(hits),
        "recall": len(hits) / len(probes),
        "nonexact_probes": len(nonexact),
        "nonexact_retrieved": nonexact_hits,
        "nonexact_recall": nonexact_hits / len(nonexact),
        "channel_hits": dict(channel_hits),
        "channel_exclusive_hits": dict(exclusive_hits),
        "by_type": sliced(probes, hits, "entry_type"),
        "by_script": sliced(probes, hits, "script_relation"),
        "by_exact_alias_overlap": sliced(probes, hits, "exact_alias_overlap"),
        "semantic": semantic is not None,
        "missed": missed,
    }
    Path(args.output).write_text(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n")
    print(
        f"views={len(views)} candidates={len(candidates)} probes={len(probes)} "
        f"recall={len(hits)}/{len(probes)} ({len(hits)/len(probes):.2%}) "
        f"nonexact={nonexact_hits}/{len(nonexact)} ({nonexact_hits/len(nonexact):.2%})"
    )
    print(f"report={args.output}")


if __name__ == "__main__":
    main()
