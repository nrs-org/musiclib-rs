# Dedup PoC

Read-only proof of concept for the entry-level dedup plan. It loads the full
3,102-entry v3 corpus, generates a bounded union of exact,
romanized, token, character-ngram, identifier, tracklist, and in-process semantic
candidates, then evaluates small type-specific logistic baselines with grouped
out-of-fold predictions and scores the generated candidate pool.

From the repository root:

```bash
nix-shell -p uv --run \
  'uv run --project poc/dedup-poc --extra semantic dedup-poc'
xdg-open data/dedup-entry-pilot-v3-poc/report.html
```

The app writes `report.json`, `candidates.csv`, a self-contained `report.html`,
`model.json`, and `runtime-model.json`. The first model keeps every research
feature; `runtime-model.json` is restricted to fields available from Rust's
`EntryInfo` and is the only artifact accepted by `softmatch --model`. Both hold
per-type full-fit coefficients/intercepts and defer-band thresholds; the
headline metrics still come from grouped out-of-fold predictions. The HTML
contains every labeled task and the 5,000 highest-scored candidates; the CSV and
JSON retain the complete candidate set. It never writes to the music database
or applies merges.

The PoC is data-pure: it reads only the entity records exported by musiclib-rs.
It does not fetch provider pages, query MusicBrainz, or silently consume caches
created from either source. External enrichment belongs in musiclib-rs and must
be present in its exported records before this evaluator can use it.

Semantic vectors are computed directly inside the process with LaBSE and queried
with a per-entry-type HNSW index; there is no embedding HTTP service or all-pairs
semantic scan. Runtime model loading is offline by default. On a new
machine, add `--allow-model-download` once to fetch the immutable model asset;
later runs use Hugging Face's local cache. `--no-semantic` runs the lexical
baseline without installing the optional ML dependencies.

The default decision thresholds target 97% observed precision independently for
automatic merge and automatic separate decisions. Pairs between the thresholds
are deferred. This is selective accuracy, not 97% forced-binary accuracy, and
the threshold is still estimated on a small diagnostic dataset. To inspect a
forced operating point without changing code:

```bash
nix-shell -p uv --run \
  'uv run --project poc/dedup-poc --extra semantic dedup-poc \
    --merge-threshold 0.5 --separate-threshold 0.2 \
    --out-dir data/dedup-poc-050'
```

The learned PoC covers identity only. Primitive relationship classification and
cluster-safe application remain downstream stages; no candidate verdict is
applied to the library.

The source-linker-grounded retrieval harness counterfactually partitions each
multi-source entry into two pseudo-entries and measures whether candidate
generation reunites them among real distractors:

```bash
python3 scripts/export_dedup_retrieval_probes.py
nix-shell -p uv --run \
  'uv run --project poc/dedup-poc --extra semantic dedup-retrieval-benchmark'
```

These probes test recall and do not estimate duplicate prevalence. Apparent
mixed source clusters must be audited before treating the panel as final gold.

Run tests:

```bash
nix-shell -p uv --run \
  'uv run --project poc/dedup-poc python -m unittest discover \
    -s poc/dedup-poc/tests'
```

The 516 labeled items were designed for calibration and failure discovery. The
known positives were drawn from retrieval-related frames, so their 100%
candidate recall is selection-biased. Metrics from this app are regression
diagnostics, not representative deployment guarantees.
