// Learned soft-dedup verdicts: the v17 + rel-v8 pair classifier
// (docs/plan-learned-matcher.md, docs/plan-v15-runtime.md).
//
// The model runs in the inference cdylib; this script binds it, feeds it pair
// facts, and owns the POLICY: merge thresholds, the DEFER band, how relation
// heads become relate() metadata. Edit the policy here, not in Rust.
//
// Setup:
//   cargo build -p inference --release --lib --features matcher   # or `vulkan`: GPU blocking vectors
//   (cd train/learned-matcher && uv run python bundle.py)   # → data/learned-matcher/bundle/v17
//   cargo build --release --features ffi
//   MUSICLIB_MATCHER_BUNDLE=$PWD/data/learned-matcher/bundle/v17 \
//     cargo run --release --features ffi --bin softmatch -- \
//       --script config/match.learned.rhai
//
// Semantic blocking (top-k neighbours) uses the matcher's title encoder (256-d)
// on `title [A] artist` for tracks, the input it was trained on. The embedding
// cache records the encoder's id (embedding_model_id), so switching to or from
// another script re-embeds the cache instead of mixing vector spaces.
//
// Verdicts:
//   MERGE    p_same ≥ the type's threshold (soft, reversible same_identity)
//   DEFER    generic/placeholder name above the threshold, or p_same leading
//            below it; refine() asks Jev about these (see below)
//   RELATE   derived_from with the kind head's transformation and, when the
//            direction head is confident, derived_side
//   DISTINCT unrelated, or siblings (two versions of one song: no direct edge)
//
// Jev (config/jev.rhai, keep it next to this script): with TYPESAFE_API_KEY set
// and libinference.so built with `--features matcher,typesafe`, refine() sends
// every DEFER pair to TypeSafe's Jev and uses its answer instead. Answers are
// cached in <cache_dir>/typesafe.db; init() logs whether Jev is on and why not.
//
// CSV: with MUSICLIB_MATCH_CSV=<path>, every scored pair gets a row with its
// final verdict and the model's outputs (see csv_header()), for analysis.

// Per-type merge thresholds on p_same: chosen on dev at 98% merge precision
// (99% for artists) by train.py, models/v17/thresholds.json. Policy, not
// model: raise one to merge less. Rhai top-level consts aren't visible inside
// functions, so these live in init()'s context.
fn thresholds() {
    #{
        track: 0.828,
        artist: 0.976,
        release: 0.994,
        release_group: 0.787,
    }
}

// The direction head is ~96.5% right when |p - 0.5| > 0.3 (dev), ~90% overall.
fn direction_margin() { 0.3 }

// The matcher and Jev open independently: either can be missing without
// taking the other down. Without the matcher every pair comes out DISTINCT,
// so report that loudly.
fn init() {
    let ctx = #{};
    try {
        ctx = open_matcher();
    } catch (err) {
        print("match.learned.rhai: matcher unavailable, every pair will be DISTINCT: " + err);
    }
    import "jev" as jev;
    ctx.jev = jev::open(if "lib" in ctx { ctx.lib } else { () });
    let csv_path = env_var("MUSICLIB_MATCH_CSV");
    if csv_path != "" {
        try {
            import "csv" as csv;   // also caches the module before decide() runs
            ctx.csv = file_create(csv_path);
            ctx.csv.write_line(csv::row(csv_header()));
            print("match.learned.rhai: writing one CSV row per scored pair to " + csv_path);
        } catch (err) {
            print("match.learned.rhai: no CSV: " + err);
        }
    }
    ctx
}

fn has_matcher(ctx) { type_of(ctx) == "map" && "h" in ctx }

fn open_matcher() {
    if !ffi_available() {
        throw "match.learned.rhai needs musiclib built with --features ffi";
    }
    let candidates = ["./libinference.so", "../target/release/libinference.so", "libinference.so"];
    let lib = ();
    let opened = false;
    for path in candidates {
        try { lib = ffi::open(path); opened = true; break; } catch (err) {}
    }
    if !opened { throw "libinference.so not found (script dir, ../target/release, system path)"; }

    let free_str = lib.func("inference_free_string", "void", ["ptr"]);
    let m = #{
        lib: lib,
        open:        lib.func("inference_matcher_open", "ptr", ["str"]),
        last_error:  lib.func("inference_matcher_last_error", "str", []),
        n_outputs:   lib.func("inference_matcher_n_outputs", "i64", ["ptr"]),
        output_name: lib.func("inference_matcher_output_name", "str", ["ptr", "i64"]),
        put_facts:   lib.func("inference_matcher_put_facts", "i32", ["ptr", "str"]),
        missing:     lib.func("inference_matcher_missing", "str", ["ptr", "str"]).freed_by(free_str),
        score:       lib.func("inference_matcher_score", "i32", ["ptr", "str", "str", "str", "ptr"]),
        prepare:     lib.func("inference_matcher_prepare", "i32", ["ptr", "str", "ptr"]),
        score_views: lib.func("inference_matcher_score_views", "i32", ["ptr", "i64", "i64", "i64", "ptr"]),
        embed_batch: lib.func("inference_matcher_embed_batch", "i32", ["ptr", "ptr", "u64", "ptr", "ptr"]),
        encoder_id:  lib.func("inference_matcher_encoder_id", "str", ["ptr"]).freed_by(free_str),
        entry_text:  lib.func("inference_matcher_entry_text", "str", ["ptr", "str", "str", "str"]).freed_by(free_str),
        free_floats: lib.func("inference_free_float_array", "void", ["ptr", "u64"]),
    };

    let bundle = env_var("MUSICLIB_MATCHER_BUNDLE");
    if bundle == "" { bundle = "data/learned-matcher/bundle/v17"; }   // relative to the CWD
    let h = m.open.invoke([bundle]);
    if h.is_null() {
        throw "inference_matcher_open(" + bundle + "): " + m.last_error.invoke();
    }
    m.h = h;

    // Output columns by name, so a bundle with another layout fails loudly.
    let n = m.n_outputs.invoke([h]);
    let col = #{};
    for i in 0..n { col[m.output_name.invoke([h, i])] = i; }
    for need in ["p_same", "p_related", "p_sibling", "p_unrelated", "guard", "p_sibling_structure", "p_a_derived"] {
        if !(need in col) { throw "matcher bundle has no output " + need; }
    }
    let kinds = [];
    for i in 0..n {
        let name = m.output_name.invoke([h, i]);
        if name.starts_with("p_kind_") { kinds.push([name.sub_string(7), i]); }
    }
    m.n = n;
    m.col = col;
    m.kinds = kinds;
    m.thresholds = thresholds();
    m
}

fn destroy(ctx) {
    if has_matcher(ctx) {
        ctx.lib.func("inference_matcher_close", "void", ["ptr"]).invoke([ctx.h]);
    }
    if type_of(ctx) == "map" && "jev" in ctx {
        import "jev" as jev;
        jev::close(ctx.jev);
    }
}

// Send the facts of any pair the matcher hasn't seen (once per library pair).
fn feed_missing(ctx, pairs_json) {
    let s = ctx.missing.invoke([ctx.h, pairs_json]);
    if s == () { throw "inference_matcher_missing: " + ctx.last_error.invoke(); }
    let missing = parse_json(s);
    if missing.len() == 0 { return; }
    let facts = pair_facts_json(missing);
    if type_of(facts) != "string" { throw "pair_facts_json unavailable (in-memory DB?)"; }
    if ctx.put_facts.invoke([ctx.h, facts]) != 0 {
        throw "inference_matcher_put_facts: " + ctx.last_error.invoke();
    }
}

// Host hook, called in batches before scoring: build every entry's view in
// the matcher (facts fed for the whole batch at once, all new texts encoded
// in one pass) and return its handle, which decide() reads as `a.prepared`.
fn prepare(ctx, entries) {
    if !has_matcher(ctx) { return entries.map(|e| ()); }
    let pairs = [];
    let items = [];
    for e in entries {
        pairs += e.pairs;
        items.push([e.entry_type, e.pairs]);
    }
    feed_missing(ctx, to_json(pairs));
    let n = entries.len();
    let out = ffi::buf(8 * n);
    if ctx.prepare.invoke([ctx.h, to_json(items), out]) != 0 {
        throw "inference_matcher_prepare: " + ctx.last_error.invoke();
    }
    ffi::read_i64(out, n)
}

// Output-row heads (inference_matcher_score_views): the main classifier with
// the guard, and the relation heads (structure, direction, kind), which are
// most of the trees and only matter on the RELATE path.
fn head_main() { 1 }
fn head_relation() { 2 }

// One output row (array of floats, see init()'s `col`) for a same-type pair,
// with the `heads` asked for filled in (all of them for unprepared entries).
// The host may call decide() from several threads at once, so each call
// gets its own output buffer.
fn score(ctx, a, b, heads) {
    let out = ffi::buf(8 * ctx.n);
    if type_of(a.prepared) == "i64" && type_of(b.prepared) == "i64" {
        if ctx.score_views.invoke([ctx.h, a.prepared, b.prepared, heads, out]) != 0 {
            throw "inference_matcher_score_views: " + ctx.last_error.invoke();
        }
    } else {
        let ja = to_json(a.pairs);
        let jb = to_json(b.pairs);
        let rc = ctx.score.invoke([ctx.h, a.entry_type, ja, jb, out]);
        if rc == 2 {
            feed_missing(ctx, ja);
            feed_missing(ctx, jb);
            rc = ctx.score.invoke([ctx.h, a.entry_type, ja, jb, out]);
        }
        if rc != 0 { throw "inference_matcher_score: " + ctx.last_error.invoke(); }
    }
    ffi::read_f64(out, ctx.n)
}

fn decide(ctx, a, b) {
    let v = judge(ctx, a, b);
    // DISTINCT pairs never reach refine(), so their CSV rows are written here.
    if "csv" in ctx && v.verdict == "distinct" { emit_row(ctx, a, b, v); }
    v
}

// The verdict. With a CSV open, it carries the model's outputs as `diag`.
fn judge(ctx, a, b) {
    // Cross-type candidates never reach the model (it was not trained on them).
    if a.entry_type != b.entry_type { return distinct(); }
    if !has_matcher(ctx) { return distinct(); }
    let o = score(ctx, a, b, head_main());
    let c = ctx.col;
    let p_same = o[c.p_same];
    let threshold = if a.entry_type in ctx.thresholds { ctx.thresholds[a.entry_type] } else { 1.0 };
    let diag = if "csv" in ctx {
        #{
            p_same: p_same, p_related: o[c.p_related], p_sibling: o[c.p_sibling],
            p_unrelated: o[c.p_unrelated], guard: o[c.guard],
        }
    } else { () };

    if p_same >= threshold {
        if o[c.guard] > 0.0 { return with_diag(defer(p_same, "learned: generic or placeholder name"), diag); }
        return with_diag(merge(p_same, "learned v17"), diag);
    }
    let best = "related";
    let best_p = o[c.p_related];
    if o[c.p_sibling] > best_p { best = "sibling"; best_p = o[c.p_sibling]; }
    if o[c.p_unrelated] > best_p { best = "unrelated"; best_p = o[c.p_unrelated]; }
    if p_same > best_p { return with_diag(defer(p_same, "learned: same leads below the merge threshold"), diag); }
    if best != "related" { return with_diag(distinct(), diag); }

    o = score(ctx, a, b, head_relation());
    let kind = "";
    let kind_p = -1.0;
    for k in ctx.kinds { if o[k[1]] > kind_p { kind = k[0]; kind_p = o[k[1]]; } }
    let p_struct = o[c.p_sibling_structure];
    let p_a = o[c.p_a_derived];
    if type_of(diag) == "map" {
        diag.p_sibling_structure = p_struct;
        diag.p_a_derived = p_a;
        diag.kind = kind;
        diag.p_kind = kind_p;
    }
    // Structure head: neither side derives from the other → siblings, no edge.
    if a.entry_type == "track" && p_struct > 0.5 { return with_diag(distinct(), diag); }

    let meta = #{};
    if kind != "other" { meta.transformation = kind; }
    if p_a - 0.5 > direction_margin() { meta.derived_side = "a"; }
    if 0.5 - p_a > direction_margin() { meta.derived_side = "b"; }
    with_diag(relate("derived_from", best_p, "learned v17 (" + kind + ")", meta), diag)
}

fn with_diag(v, diag) {
    if type_of(diag) == "map" { v.diag = diag; }
    v
}

// Host hook, after decide(): items are `#{a, b, verdict}` for every pair that
// didn't come out DISTINCT; return one verdict per item, () to keep it.
// Policy: only the model's undecided pairs (DEFER) go to Jev; a confident
// MERGE or RELATE stands.
fn refine(ctx, items) {
    let out = items.map(|it| ());
    let idx = [];
    let ask = [];
    if type_of(ctx.jev) == "map" {
        for i in 0..items.len() {
            if items[i].verdict.verdict == "defer" {
                idx.push(i);
                ask.push(items[i]);
            }
        }
    }
    if !ask.is_empty() {
        import "jev" as jev;
        try {
            let answers = jev::refine_pairs(ctx.jev, ask);
            for k in 0..idx.len() { out[idx[k]] = answers[k]; }
        } catch (err) {
            print("jev: batch of " + ask.len() + " failed, keeping the model's verdicts: " + err);
        }
    }
    if "csv" in ctx {
        for i in 0..items.len() {
            let it = items[i];
            let v = if type_of(out[i]) == "()" { it.verdict } else { out[i] };
            if !("diag" in v) && "diag" in it.verdict { v.diag = it.verdict.diag; }
            emit_row(ctx, it.a, it.b, v);
        }
    }
    out
}

// ── CSV (MUSICLIB_MATCH_CSV) ─────────────────────────────────────────────────
//
// One row per scored pair, for analysis: written from decide() for DISTINCT
// pairs (in parallel) and from refine() for the rest, with the final verdict.
// Rows follow the scoring threads, not pair order. Pairs the host skips before
// scoring (already linked, marked different, dedup barrier) have no row.
// Quoting is config/csv.rhai (keep it next to this script).

fn csv_header() {
    [
        "verdict", "kind", "confidence", "reason", "origin", "model_version", "metadata",
        "type", "entry_a", "title_a", "sources_a", "entry_b", "title_b", "sources_b",
        "p_same", "p_related", "p_sibling", "p_unrelated", "guard",
        "p_sibling_structure", "p_a_derived", "rel_kind", "p_rel_kind",
    ]
}

fn emit_row(ctx, a, b, v) {
    import "csv" as csv;
    let d = if "diag" in v { v.diag } else { #{} };
    ctx.csv.write_line(csv::row([
        v.verdict, v.kind, v.confidence, v.reason,
        if "origin" in v { v.origin } else { "heuristic" }, v.model_version,
        if "metadata" in v { to_json(v.metadata) } else { () },
        a.entry_type, a.entry_id, a.title, csv_sources(a), b.entry_id, b.title, csv_sources(b),
        d.p_same, d.p_related, d.p_sibling, d.p_unrelated, d.guard,
        d.p_sibling_structure, d.p_a_derived, d.kind, d.p_kind,
    ]));
}

fn csv_sources(e) {
    let parts = [];
    for p in e.pairs { parts.push(p.source + ":" + p.identifier); }
    join(parts, " ")
}

// Semantic blocking: the cache's model id. Vectors from any other id are
// dropped and re-embedded.
fn embedding_model_id(ctx) {
    if !has_matcher(ctx) { throw "matcher unavailable: no embeddings"; }
    let id = ctx.encoder_id.invoke([ctx.h]);
    if id == () { throw "inference_matcher_encoder_id: " + ctx.last_error.invoke(); }
    if id == "" { throw "matcher bundle has no manifest.json encoder id"; }
    "learned-matcher:" + id
}

// Semantic blocking: the text an entry is embedded from. Tracks get
// `title [A] primary artist` (from the pair facts); other types their title.
fn embed_text(ctx, entry) {
    if !has_matcher(ctx) || entry.entry_type != "track" { return entry.title; }
    let pj = to_json(entry.pairs);
    feed_missing(ctx, pj);
    let text = ctx.entry_text.invoke([ctx.h, entry.entry_type, entry.title, pj]);
    if text == () { throw "inference_matcher_entry_text: " + ctx.last_error.invoke(); }
    text
}

// Semantic blocking: title-encoder vectors (256-d, L2-normalised).
fn embed_batch(ctx, texts) {
    if !has_matcher(ctx) { throw "matcher unavailable: no embeddings"; }
    let n = texts.len();
    if n == 0 { return []; }
    let out_flat = ffi::buf(8);
    let out_dim = ffi::buf(8);
    let rc = ctx.embed_batch.invoke([ctx.h, ffi::cstr_array(texts), n, out_flat, out_dim]);
    if rc != 0 { throw "inference_matcher_embed_batch: " + ctx.last_error.invoke(); }
    let dim = ffi::read_i64(out_dim, 1)[0];
    let flat = ffi::read_ptr(out_flat, 0);
    let all = ffi::read_f32(flat, n * dim);
    ctx.free_floats.invoke([flat, n * dim]);
    let rows = [];
    for i in 0..n { rows.push(all.extract(i * dim, dim)); }
    rows
}

fn embed(ctx, text) {
    let r = embed_batch(ctx, [text]);
    if r.is_empty() { [] } else { r[0] }
}
