from __future__ import annotations

import argparse
import csv
import json
from pathlib import Path

from .core import (
    RUNTIME_FEATURE_NAMES,
    cross_validated_probabilities,
    evaluate,
    generate_candidates,
    load_dataset,
)
from .report import write_report
from .embeddings import DEFAULT_MODEL, embed_views


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description="Read-only musiclib deduplication PoC")
    result.add_argument("--tasks", default="data/dedup-entry-pilot-v3.private.jsonl")
    result.add_argument("--lane-a", default="data/dedup-entry-pilot-v3-labels/lane-a")
    result.add_argument("--lane-b", default="data/dedup-entry-pilot-v3-labels/lane-b")
    result.add_argument("--adjudications", default="data/dedup-entry-pilot-v3-adjudications.jsonl")
    result.add_argument("--corpus-views", default="data/dedup-entry-pilot-v3.corpus-views.jsonl")
    result.add_argument("--out-dir", default="data/dedup-entry-pilot-v3-poc")
    result.add_argument("--max-block", type=int, default=50)
    result.add_argument("--ngram-k", type=int, default=30)
    result.add_argument(
        "--candidate-k", type=int, default=0,
        help="optional global per-entry union cap; 0 keeps channel-level bounds",
    )
    result.add_argument("--no-semantic", action="store_true")
    result.add_argument("--embedding-model", default=DEFAULT_MODEL)
    result.add_argument("--allow-model-download", action="store_true")
    result.add_argument("--embedding-batch-size", type=int, default=32)
    result.add_argument("--semantic-k", type=int, default=20)
    result.add_argument("--semantic-threshold", type=float, default=0.45)
    result.add_argument("--folds", type=int, default=5)
    result.add_argument("--merge-threshold", type=float)
    result.add_argument("--separate-threshold", type=float)
    result.add_argument("--target-precision", type=float, default=0.97)
    return result


def main() -> None:
    args = parser().parse_args()
    views, pairs = load_dataset(
        args.tasks,
        args.lane_a,
        args.lane_b,
        args.adjudications,
        corpus_views_path=args.corpus_views,
    )
    embeddings = None if args.no_semantic else embed_views(
        views,
        model_name=args.embedding_model,
        batch_size=args.embedding_batch_size,
        allow_download=args.allow_model_download,
    )
    candidates = generate_candidates(
        views,
        max_block=args.max_block,
        ngram_k=args.ngram_k,
        semantic=embeddings,
        semantic_k=args.semantic_k,
        semantic_threshold=args.semantic_threshold,
        candidate_k=args.candidate_k,
    )
    probabilities, model = cross_validated_probabilities(pairs, views, args.folds)
    report = evaluate(
        views,
        pairs,
        candidates,
        probabilities,
        model,
        merge_threshold=args.merge_threshold,
        separate_threshold=args.separate_threshold,
        target_precision=args.target_precision,
    )
    runtime_probabilities, runtime_models = cross_validated_probabilities(
        pairs, views, args.folds, RUNTIME_FEATURE_NAMES
    )
    runtime_report = evaluate(
        views,
        pairs,
        candidates,
        runtime_probabilities,
        runtime_models,
        merge_threshold=args.merge_threshold,
        separate_threshold=args.separate_threshold,
        target_precision=args.target_precision,
    )
    runtime_model = runtime_report["scoring"]["model"]
    runtime_model["profile"] = "musiclib-entry-info/1"
    runtime_model["embedding"] = {
        "required": "semantic_similarity" in runtime_model["features"],
        "model": args.embedding_model if embeddings is not None else None,
    }
    report["runtime_scoring"] = {
        **{key: value for key, value in runtime_report["scoring"].items() if key != "model"},
        "model_file": "runtime-model.json",
    }
    report["embedding"] = {
        "enabled": embeddings is not None,
        "model": args.embedding_model if embeddings is not None else None,
        "model_download_allowed": args.allow_model_download,
        "input_source": "musiclib-rs exported records only",
    }
    report["candidate_generation"] = {
        "max_block": args.max_block,
        "ngram_k": args.ngram_k,
        "global_candidate_k_per_entry": args.candidate_k or None,
        "channel_level_bounds_preserved": args.candidate_k == 0,
        "semantic_k": args.semantic_k if embeddings is not None else None,
        "semantic_threshold": args.semantic_threshold if embeddings is not None else None,
        "semantic_index": "hnsw-cosine" if embeddings is not None else None,
    }
    output = Path(args.out_dir)
    output.mkdir(parents=True, exist_ok=True)
    (output / "report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    (output / "model.json").write_text(
        json.dumps(report["scoring"]["model"], ensure_ascii=False, indent=2, sort_keys=True)
        + "\n",
        encoding="utf-8",
    )
    (output / "runtime-model.json").write_text(
        json.dumps(runtime_model, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    with (output / "candidates.csv").open("w", encoding="utf-8", newline="") as handle:
        columns = [
            "short_id", "type", "left_title", "right_title", "channels",
            "probability_same", "prediction", "gold", "item_id",
            "left_view_id", "right_view_id",
        ]
        writer = csv.DictWriter(handle, fieldnames=columns)
        writer.writeheader()
        for row in report["candidates"]:
            writer.writerow(
                {
                    **{column: row.get(column) for column in columns},
                    "channels": ";".join(row["channels"]),
                }
            )
    write_report(report, output / "report.html")
    dataset = report["dataset"]
    retrieval = report["retrieval"]
    scoring = report["scoring"]
    print(f"tasks={dataset['tasks']} unique_views={dataset['unique_views']} labels={dataset['labels']}")
    print(
        f"retrieval candidates={retrieval['candidate_pairs']}/{retrieval['possible_same_type_pairs']} "
        f"({retrieval['candidate_fraction']:.2%}) positive_recall="
        f"{retrieval['positives_retrieved']}/{retrieval['positives_total']} "
        f"({retrieval['positive_recall']:.2%})"
    )
    def percent(value: float | None) -> str:
        return "n/a" if value is None else f"{value:.2%}"

    print(
        f"scoring oof_decided_accuracy={scoring['decided_accuracy']:.2%} "
        f"merge_precision={percent(scoring['auto_merge_precision'])} "
        f"separate_precision={percent(scoring['auto_separate_precision'])} "
        f"oof_merge_recall={scoring['auto_merge_recall']:.2%} "
        f"coverage={scoring['coverage']:.2%} thresholds="
        f"separate<={scoring['separate_threshold']:.4f},merge>={scoring['merge_threshold']:.4f}"
    )
    print(
        f"report={output / 'report.html'} candidates={output / 'candidates.csv'} "
        f"model={output / 'model.json'} runtime_model={output / 'runtime-model.json'}"
    )


if __name__ == "__main__":
    main()
