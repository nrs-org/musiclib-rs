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
//            below it (counted, CSV, Jev with --jev-types)
//   RELATE   derived_from with the kind head's transformation and, when the
//            direction head is confident, derived_side
//   DISTINCT unrelated, or siblings (two versions of one song: no direct edge)

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

// The host treats an init() error as "no context" (every pair would come out
// DISTINCT), so report it loudly first.
fn init() {
    try {
        return open_matcher();
    } catch (err) {
        print("match.learned.rhai: matcher unavailable, every pair will be DISTINCT: " + err);
    }
    ()
}

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

    let m = #{
        lib: lib,
        open:        lib.func("inference_matcher_open", "ptr", ["ptr"]),
        last_error:  lib.func("inference_matcher_last_error", "ptr", []),
        n_outputs:   lib.func("inference_matcher_n_outputs", "i64", ["ptr"]),
        output_name: lib.func("inference_matcher_output_name", "ptr", ["ptr", "i64"]),
        put_facts:   lib.func("inference_matcher_put_facts", "i32", ["ptr", "ptr"]),
        missing:     lib.func("inference_matcher_missing", "ptr", ["ptr", "ptr"]),
        score:       lib.func("inference_matcher_score", "i32", ["ptr", "ptr", "ptr", "ptr", "ptr"]),
        prepare:     lib.func("inference_matcher_prepare", "i32", ["ptr", "ptr", "ptr"]),
        score_views: lib.func("inference_matcher_score_views", "i32", ["ptr", "i64", "i64", "i64", "ptr"]),
        embed_batch: lib.func("inference_matcher_embed_batch", "i32", ["ptr", "ptr", "u64", "ptr", "ptr"]),
        encoder_id:  lib.func("inference_matcher_encoder_id", "ptr", ["ptr"]),
        entry_text:  lib.func("inference_matcher_entry_text", "ptr", ["ptr", "ptr", "ptr", "ptr"]),
        free_str:    lib.func("inference_free_string", "void", ["ptr"]),
        free_floats: lib.func("inference_free_float_array", "void", ["ptr", "u64"]),
    };

    let bundle = env_var("MUSICLIB_MATCHER_BUNDLE");
    if bundle == "" { bundle = "data/learned-matcher/bundle/v17"; }   // relative to the CWD
    let cs = ffi::cstr(bundle);
    let h = m.open.invoke([cs]);
    ffi::free(cs);
    if h.is_null() {
        throw "inference_matcher_open(" + bundle + "): " + ffi::read_cstr(m.last_error.invoke([]));
    }
    m.h = h;

    // Output columns by name, so a bundle with another layout fails loudly.
    let n = m.n_outputs.invoke([h]);
    let col = #{};
    for i in 0..n { col[ffi::read_cstr(m.output_name.invoke([h, i]))] = i; }
    for need in ["p_same", "p_related", "p_sibling", "p_unrelated", "guard", "p_sibling_structure", "p_a_derived"] {
        if !(need in col) { throw "matcher bundle has no output " + need; }
    }
    let kinds = [];
    for i in 0..n {
        let name = ffi::read_cstr(m.output_name.invoke([h, i]));
        if name.starts_with("p_kind_") { kinds.push([name.sub_string(7), i]); }
    }
    m.n = n;
    m.col = col;
    m.kinds = kinds;
    m.thresholds = thresholds();
    m
}

fn destroy(ctx) {
    if type_of(ctx) == "map" {
        ctx.lib.func("inference_matcher_close", "void", ["ptr"]).invoke([ctx.h]);
    }
}

// Send the facts of any pair the matcher hasn't seen (once per library pair).
fn feed_missing(ctx, pairs_json) {
    let cs = ffi::cstr(pairs_json);
    let p = ctx.missing.invoke([ctx.h, cs]);
    ffi::free(cs);
    if p.is_null() { throw "inference_matcher_missing: " + ffi::read_cstr(ctx.last_error.invoke([])); }
    let missing = parse_json(ffi::read_cstr(p));
    ctx.free_str.invoke([p]);
    if missing.len() == 0 { return; }
    let facts = pair_facts_json(missing);
    if type_of(facts) != "string" { throw "pair_facts_json unavailable (in-memory DB?)"; }
    let cf = ffi::cstr(facts);
    let rc = ctx.put_facts.invoke([ctx.h, cf]);
    ffi::free(cf);
    if rc != 0 { throw "inference_matcher_put_facts: " + ffi::read_cstr(ctx.last_error.invoke([])); }
}

// Host hook, called in batches before scoring: build every entry's view in
// the matcher (facts fed for the whole batch at once, all new texts encoded
// in one pass) and return its handle, which decide() reads as `a.prepared`.
fn prepare(ctx, entries) {
    if type_of(ctx) != "map" { return entries.map(|e| ()); }
    let pairs = [];
    let items = [];
    for e in entries {
        pairs += e.pairs;
        items.push([e.entry_type, e.pairs]);
    }
    feed_missing(ctx, to_json(pairs));
    let n = entries.len();
    let cs = ffi::cstr(to_json(items));
    let out = ffi::malloc(8 * n);
    let rc = ctx.prepare.invoke([ctx.h, cs, out]);
    ffi::free(cs);
    if rc != 0 {
        ffi::free(out);
        throw "inference_matcher_prepare: " + ffi::read_cstr(ctx.last_error.invoke([]));
    }
    let handles = ffi::read_i64(out, n);
    ffi::free(out);
    handles
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
    let out = ffi::malloc(8 * ctx.n);
    let rc = 0;
    if type_of(a.prepared) == "i64" && type_of(b.prepared) == "i64" {
        rc = ctx.score_views.invoke([ctx.h, a.prepared, b.prepared, heads, out]);
        if rc != 0 {
            ffi::free(out);
            throw "inference_matcher_score_views: " + ffi::read_cstr(ctx.last_error.invoke([]));
        }
    } else {
        let ja = to_json(a.pairs);
        let jb = to_json(b.pairs);
        let ct = ffi::cstr(a.entry_type);
        let ca = ffi::cstr(ja);
        let cb = ffi::cstr(jb);
        rc = ctx.score.invoke([ctx.h, ct, ca, cb, out]);
        if rc == 2 {
            feed_missing(ctx, ja);
            feed_missing(ctx, jb);
            rc = ctx.score.invoke([ctx.h, ct, ca, cb, out]);
        }
        ffi::free(ct); ffi::free(ca); ffi::free(cb);
        if rc != 0 {
            ffi::free(out);
            throw "inference_matcher_score: " + ffi::read_cstr(ctx.last_error.invoke([]));
        }
    }
    let row = ffi::read_f64(out, ctx.n);
    ffi::free(out);
    row
}

fn decide(ctx, a, b) {
    // Cross-type candidates never reach the model (it was not trained on them).
    if a.entry_type != b.entry_type { return distinct(); }
    if type_of(ctx) != "map" { return distinct(); }
    let o = score(ctx, a, b, head_main());
    let c = ctx.col;
    let p_same = o[c.p_same];
    let threshold = if a.entry_type in ctx.thresholds { ctx.thresholds[a.entry_type] } else { 1.0 };

    if p_same >= threshold {
        if o[c.guard] > 0.0 { return defer(p_same, "learned: generic or placeholder name"); }
        return merge(p_same, "learned v17");
    }
    let best = "related";
    let best_p = o[c.p_related];
    if o[c.p_sibling] > best_p { best = "sibling"; best_p = o[c.p_sibling]; }
    if o[c.p_unrelated] > best_p { best = "unrelated"; best_p = o[c.p_unrelated]; }
    if p_same > best_p { return defer(p_same, "learned: same leads below the merge threshold"); }
    if best != "related" { return distinct(); }

    o = score(ctx, a, b, head_relation());
    // Structure head: neither side derives from the other → siblings, no edge.
    let p_struct = o[c.p_sibling_structure];
    if a.entry_type == "track" && p_struct > 0.5 { return distinct(); }

    let meta = #{};
    let kind = "";
    let kind_p = -1.0;
    for k in ctx.kinds { if o[k[1]] > kind_p { kind = k[0]; kind_p = o[k[1]]; } }
    if kind != "other" { meta.transformation = kind; }
    let p_a = o[c.p_a_derived];
    if p_a - 0.5 > direction_margin() { meta.derived_side = "a"; }
    if 0.5 - p_a > direction_margin() { meta.derived_side = "b"; }
    relate("derived_from", best_p, "learned v17 (" + kind + ")", meta)
}

// Semantic blocking: the cache's model id. Vectors from any other id are
// dropped and re-embedded.
fn embedding_model_id(ctx) {
    if type_of(ctx) != "map" { throw "matcher unavailable: no embeddings"; }
    let p = ctx.encoder_id.invoke([ctx.h]);
    if p.is_null() { throw "inference_matcher_encoder_id: " + ffi::read_cstr(ctx.last_error.invoke([])); }
    let id = ffi::read_cstr(p);
    ctx.free_str.invoke([p]);
    if id == "" { throw "matcher bundle has no manifest.json encoder id"; }
    "learned-matcher:" + id
}

// Semantic blocking: the text an entry is embedded from. Tracks get
// `title [A] primary artist` (from the pair facts); other types their title.
fn embed_text(ctx, entry) {
    if type_of(ctx) != "map" || entry.entry_type != "track" { return entry.title; }
    let pj = to_json(entry.pairs);
    feed_missing(ctx, pj);
    let ct = ffi::cstr(entry.entry_type);
    let cti = ffi::cstr(entry.title);
    let cp = ffi::cstr(pj);
    let p = ctx.entry_text.invoke([ctx.h, ct, cti, cp]);
    ffi::free(ct); ffi::free(cti); ffi::free(cp);
    if p.is_null() { throw "inference_matcher_entry_text: " + ffi::read_cstr(ctx.last_error.invoke([])); }
    let text = ffi::read_cstr(p);
    ctx.free_str.invoke([p]);
    text
}

// Semantic blocking: title-encoder vectors (256-d, L2-normalised).
fn embed_batch(ctx, texts) {
    if type_of(ctx) != "map" { throw "matcher unavailable: no embeddings"; }
    let n = texts.len();
    if n == 0 { return []; }
    let argv = ffi::malloc(8 * n);
    let cstrs = [];
    for i in 0..n {
        let cs = ffi::cstr(texts[i]);
        cstrs.push(cs);
        ffi::write_ptr(argv, i, cs);
    }
    let out_flat = ffi::malloc(8);
    let out_dim = ffi::malloc(8);
    let rc = ctx.embed_batch.invoke([ctx.h, argv, n, out_flat, out_dim]);
    let rows = [];
    if rc == 0 {
        let dim = ffi::read_i64(out_dim, 1)[0];
        let flat = ffi::read_ptr(out_flat, 0);
        let all = ffi::read_f32(flat, n * dim);
        for i in 0..n { rows.push(all.extract(i * dim, dim)); }
        ctx.free_floats.invoke([flat, n * dim]);
    }
    for cs in cstrs { ffi::free(cs); }
    ffi::free(argv); ffi::free(out_flat); ffi::free(out_dim);
    if rc != 0 { throw "inference_matcher_embed_batch: " + ffi::read_cstr(ctx.last_error.invoke([])); }
    rows
}

fn embed(ctx, text) {
    let r = embed_batch(ctx, [text]);
    if r.is_empty() { [] } else { r[0] }
}

// CSV diagnostics hooks (softmatch --csv). The learned model has no single
// title-similarity number; report neutral values.
fn main_title_sim(ctx, a, b) { 0.0 }
fn markers_conflict(ctx, a, b) { false }
