// match.rhai — heuristic soft-dedup policy.
// Copy to <config_dir>/match.rhai and tune thresholds as needed.
//
// Lifecycle:  init()             called once after the script is loaded (optional);
//                                its return value is `ctx` — an arbitrary context
//                                object threaded into every hook below
//             decide(ctx, a, b)  called for each candidate pair
//             destroy(ctx)       called when the engine shuts down (optional)
//
// Context object (`ctx`): whatever init() returns, the host holds for the whole
// run and passes back as the first argument of decide / embed_batch / embed /
// main_title_sim / markers_conflict. Use it to carry state that must outlive a
// single call and cross Rhai's isolated function scopes — e.g. an ffi-opened
// library handle (this script stores the inference handle there). It's () when
// init() is absent or returns ().
//
// Entity fields (a, b):
//   entry_type          "track" | "release" | "release_group" | "artist"
//   title               best known name (raw, unprocessed)
//   duration_ms         integer or ()
//   pairs               array of #{source, identifier} — canonical DB pairs
//   aliases             array of strings — all known names (case-dedup'd)
//   sourced_aliases     array of #{source, name} — clean sources appear before
//                       video sources (youtube, nicovideo, soundcloud)
//   peer_ids            array of integers — credited-artist entry IDs (for tracks)
//                       or credited-track entry IDs (for artists); opaque DB IDs
//   track_positions     array of #{release_id, disc_no, track_no} — placement
//                       in releases; disc_no/track_no are () when unknown
//
// Low-level string primitives (free functions):
//   normalize(s)                   → String   lowercase, alphanumeric+CJK, collapsed
//   jaccard(a, b)                  → f64      token Jaccard (on normalized strings)
//   levenshtein(a, b)              → f64      normalized Levenshtein
//   jaro_winkler(a, b)             → f64      Jaro-Winkler
//   str_sim(a, b)                  → f64      max(levenshtein, jaccard)
//
// Regex primitives (free functions):
//   compile_re(pattern)                     → Regex   compile once, store in a variable
//   re_is_match(re_or_pat, text)            → bool
//   re_replace_all(re_or_pat, repl, text)   → String  (repl is literal — $ not special)
//   re_captures_all(re_or_pat, text)        → [[String]]
//       each inner array: [full_match, group1, group2, …]; unmatched groups are ""
//   (re_or_pat accepts either a pre-compiled Regex or a pattern String)
//
// Verdict constructors:
//   merge(conf, reason)             → verdict
//   relate(kind, conf, reason)      → verdict
//   distinct()                      → verdict
//
// Valid relate kinds:
//   alt_version | live | remix | instrumental | cover | medley
//   release_variant | in_release_group | same_artist
//
// ── Semantic blocking (optional) ─────────────────────────────────────────────
//
// If you define embed_batch(ctx, texts) → array-of-arrays, Rust calls it in chunks
// of 64 at startup for every entry with a missing or stale embedding. Falls back
// to embed(ctx, text) → array if embed_batch is not defined. The vectors are stored
// in the embedding cache (default: <data_dir>/embeddings.db). Rust then uses
// sqlite-vec KNN to find near-neighbour pairs cross-language.
//
// This example defines embed_batch with TWO backends chosen at runtime: the
// local `inference` cdylib (real semantic vectors) when available, else a
// dependency-free naive token-hash embedding. See the "Semantic embedding"
// section below for how detection works and how to enable the cdylib.
// Embedding dimension must match the host's `embed_dim` config (default 256).
//
// During decide(), semantic_sim(a.entry_id, b.entry_id) returns the pre-computed
// cosine similarity [0, 1] from the cache — no HTTP call at scoring time:
//
//   fn decide_track(ctx, a, b) {
//       let sem = semantic_sim(a.entry_id, b.entry_id);
//       if sem >= 0.92 { return merge(sem, "high semantic similarity"); }
//       // ...
//   }
//
// semantic_sim returns 0.0 if either entry has no cached embedding.
//
// NOTE: Rhai functions have their own scope — define thresholds inside each
// function (not at the top level) so they are visible where they are used.

// ── State across calls ────────────────────────────────────────────────────────
//
// In Rhai, neither top-level `let`/`const` nor `global::` are visible inside
// function calls — every function has a completely isolated scope. So there is no
// hidden global state to lean on. Two patterns cover it:
//
// 1. Inline small constants where they are used (thresholds, fixed lists):
//
//      fn is_video_source(src) {
//          ["youtube", "nicovideo", "soundcloud"].contains(src)
//      }
//
// 2. For anything that must persist across calls or be shared with helpers,
//    build it once in init() and let the host thread it back as `ctx`. Pass it
//    explicitly down into the helpers that need it — exactly how this script
//    carries the inference handle (init → embed_batch → inf_embed_batch) and how
//    decide() forwards `ctx` to decide_track()/decide_release()/etc:
//
//      fn init() {
//          #{ clean_pat: compile_re("\\(([^)]*)\\)|\\[([^\\]]*)\\]|…") }
//      }
//      fn decide(ctx, a, b) {
//          let caps = re_captures_all(ctx.clean_pat, a.title);  // works anywhere
//          …
//      }

// ── Semantic embedding ────────────────────────────────────────────────────────

// Two backends, selected AT RUNTIME — no edits needed per build:
//
//   1. inference cdylib (real, cross-language semantic embeddings) — used when
//      musiclib was built with `--features ffi` AND the cdylib loads.
//   2. naive token-hash bag-of-words (dependency-free fallback) — otherwise.
//
// Detection: `ffi_available()` (host fn) reports whether the `ffi` module is
// compiled in; a `try { ffi::open(...) }` guard then handles the cdylib being
// missing/unbuilt. Build the plugin with `cargo build -p inference --release
// --lib` and musiclib with `--features ffi` to get backend 1.
//
// Both backends emit EMBED_DIM-dimensional vectors so they're interchangeable
// in the cache. Keep EMBED_DIM == the host's `embed_dim` config (default 256;
// the inference default backend, Model2Vec, is also 256-d — set both to 384 if
// you build inference with `--features minilm`).

// ── Lifecycle: init() → context object ───────────────────────────────────────
//
// The host calls init() once and threads whatever it returns back as the first
// argument (`ctx`) of every other entry point — embed_batch, embed, decide,
// main_title_sim, markers_conflict. It's an arbitrary context object owned by
// this script; the host keeps it alive for the whole run and never inspects it.
//
// We use it to open the inference library exactly once: the returned handle (a
// map of bound C functions — see open_inference) stays mapped for the run, so
// the model loads a single time instead of once per embedding batch. When ffi is
// unavailable or the library can't load we return () and every hook falls back
// to the dependency-free naive path.
fn init() {
    if ffi_available() {
        try {
            return open_inference();
        } catch (err) {
            print("inference unavailable, using naive embedding: " + err);
        }
    }
    ()
}

// Semantic blocking hook: batch embedding (array-of-arrays). `ctx` is init()'s
// return: the inference handle (a map) when available, else (). Rust calls this
// in chunks at startup; vectors feed sqlite-vec KNN candidate generation.
fn embed_batch(ctx, texts) {
    if type_of(ctx) == "map" {
        try {
            let rows = inf_embed_batch(ctx, normalize_for_embedding(ctx, texts));
            // Use inference only if it produced one vector per input; otherwise
            // (e.g. rc != 0) fall through to the naive backend.
            if rows.len() == texts.len() { return rows; }
        } catch (err) {
            print("inference embedding failed, using naive fallback: " + err);
        }
    }
    texts.map(|t| naive_embed(t))
}

// Fallback hook: single-text embedding.
fn embed(ctx, text) {
    let r = embed_batch(ctx, [text]);
    if r.is_empty() { [] } else { r[0] }
}

// ── Backend 1: inference cdylib via the `ffi` module ─────────────────────────
//
// `ffi::open` takes a full library filename (no auto extension): .so on Linux,
// .dylib on macOS, .dll on Windows. A path with a separator is resolved relative
// to THIS script file; a bare name goes through the system loader search path
// (LD_LIBRARY_PATH / ldconfig). These functions reference `ffi::*`; that's fine
// even without the feature, because `embed_batch` only calls them after
// `ffi_available()` is true. The `ffi` module can bind ANY C library — see the
// `pipeline::ffi` docs for the full surface (struct_type, callback, memory
// helpers, etc.).
//
// Called once from init(); the returned map (the bound C handles + the mapped
// library) becomes the context object the host threads back into every hook, so
// the library stays open for the whole run and its model loads only once.
fn open_inference() {
    // Try, in order: a `libinference.so` symlink/copy next to this script (e.g.
    // `ln -s …/target/release/libinference.so <config_dir>/`), then the repo build
    // tree (running from the repo: config/ → ../target), then the system loader
    // search path by bare name.
    let candidates = [
        "./libinference.so",
        "../target/release/libinference.so",
        "libinference.so",
    ];
    let lib = ();
    let opened = false;
    for path in candidates {
        try { lib = ffi::open(path); opened = true; break; } catch (err) {}
    }
    if !opened {
        throw "libinference.so not found (tried script dir, ../target/release, system path)";
    }
    #{
        detect_romaji: lib.func("inference_detect_romaji", "ptr", ["ptr", "ptr"]),
        free_strs:     lib.func("inference_free_string_array", "void", ["ptr", "u64"]),
        embed_batch:   lib.func("inference_embed_batch", "i32", ["ptr", "u64", "ptr", "ptr"]),
        free_floats:   lib.func("inference_free_float_array", "void", ["ptr", "u64"]),
        lib: lib,   // keep the library mapped while this map is alive
    }
}

// Romanised-JP tokens of `text` (marshals the char** + out-length contract).
fn inf_detect_romaji(inf, text) {
    let cs = ffi::cstr(text);
    let out_len = ffi::malloc(8);
    let arr = inf.detect_romaji.invoke([cs, out_len]);
    let toks = [];
    if !arr.is_null() {
        let n = ffi::read_i64(out_len, 1)[0];
        for i in 0..n { toks.push(ffi::read_cstr(ffi::read_ptr(arr, i))); }
        inf.free_strs.invoke([arr, n]);
    }
    ffi::free(cs);
    ffi::free(out_len);
    toks
}

// Batch embeddings (array-of-arrays): build char*[] argv, read back flat f32.
fn inf_embed_batch(inf, texts) {
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
    let rc = inf.embed_batch.invoke([argv, n, out_flat, out_dim]);
    let rows = [];
    if rc == 0 {
        let dim = ffi::read_i64(out_dim, 1)[0];
        let flat = ffi::read_ptr(out_flat, 0);
        if dim > 0 && !flat.is_null() {
            let all = ffi::read_f32(flat, n * dim);
            for i in 0..n {
                let row = [];
                for j in 0..dim { row.push(all[i * dim + j]); }
                rows.push(row);
            }
            inf.free_floats.invoke([flat, n * dim]);
        }
    }
    for cs in cstrs { ffi::free(cs); }
    ffi::free(argv);
    ffi::free(out_flat);
    ffi::free(out_dim);
    rows
}

// Romaji→hiragana so romanised JP embeds near native JP (cross-script recall).
fn normalize_for_embedding(inf, texts) {
    let normalized = texts;
    for i in 0..texts.len() {
        let t = texts[i];
        for tok in inf_detect_romaji(inf, t) {
            let h = romaji_to_hiragana(tok.to_lower());
            if !re_is_match("[a-z]", h) { t.replace(tok, h); }
        }
        normalized[i] = t;
    }
    normalized
}

fn romaji_to_hiragana(r) {
    let t = r;
    // double consonants → っ + remainder
    t.replace("kk","っk"); t.replace("ss","っs"); t.replace("tt","っt");
    t.replace("pp","っp"); t.replace("bb","っb"); t.replace("mm","っm");
    t.replace("gg","っg"); t.replace("dd","っd"); t.replace("zz","っz");
    // 3-char mora (digraphs — must come before 2-char)
    t.replace("sha","しゃ"); t.replace("shi","し"); t.replace("shu","しゅ"); t.replace("sho","しょ");
    t.replace("chi","ち"); t.replace("cha","ちゃ"); t.replace("chu","ちゅ"); t.replace("cho","ちょ");
    t.replace("tsu","つ");
    t.replace("kya","きゃ"); t.replace("kyu","きゅ"); t.replace("kyo","きょ");
    t.replace("nya","にゃ"); t.replace("nyu","にゅ"); t.replace("nyo","にょ");
    t.replace("hya","ひゃ"); t.replace("hyu","ひゅ"); t.replace("hyo","ひょ");
    t.replace("mya","みゃ"); t.replace("myu","みゅ"); t.replace("myo","みょ");
    t.replace("rya","りゃ"); t.replace("ryu","りゅ"); t.replace("ryo","りょ");
    t.replace("gya","ぎゃ"); t.replace("gyu","ぎゅ"); t.replace("gyo","ぎょ");
    t.replace("bya","びゃ"); t.replace("byu","びゅ"); t.replace("byo","びょ");
    t.replace("pya","ぴゃ"); t.replace("pyu","ぴゅ"); t.replace("pyo","ぴょ");
    t.replace("ja","じゃ"); t.replace("ji","じ"); t.replace("ju","じゅ"); t.replace("jo","じょ");
    // 2-char mora
    t.replace("ka","か"); t.replace("ki","き"); t.replace("ku","く"); t.replace("ke","け"); t.replace("ko","こ");
    t.replace("sa","さ"); t.replace("su","す"); t.replace("se","せ"); t.replace("so","そ");
    t.replace("ta","た"); t.replace("te","て"); t.replace("to","と");
    t.replace("na","な"); t.replace("ni","に"); t.replace("nu","ぬ"); t.replace("ne","ね"); t.replace("no","の");
    t.replace("ha","は"); t.replace("hi","ひ"); t.replace("fu","ふ"); t.replace("he","へ"); t.replace("ho","ほ");
    t.replace("ma","ま"); t.replace("mi","み"); t.replace("mu","む"); t.replace("me","め"); t.replace("mo","も");
    t.replace("ya","や"); t.replace("yu","ゆ"); t.replace("yo","よ");
    t.replace("ra","ら"); t.replace("ri","り"); t.replace("ru","る"); t.replace("re","れ"); t.replace("ro","ろ");
    t.replace("wa","わ"); t.replace("wo","を");
    t.replace("ga","が"); t.replace("gi","ぎ"); t.replace("gu","ぐ"); t.replace("ge","げ"); t.replace("go","ご");
    t.replace("za","ざ"); t.replace("zu","ず"); t.replace("ze","ぜ"); t.replace("zo","ぞ");
    t.replace("da","だ"); t.replace("de","で"); t.replace("do","ど");
    t.replace("ba","ば"); t.replace("bi","び"); t.replace("bu","ぶ"); t.replace("be","べ"); t.replace("bo","ぼ");
    t.replace("pa","ぱ"); t.replace("pi","ぴ"); t.replace("pu","ぷ"); t.replace("pe","ぺ"); t.replace("po","ぽ");
    // single vowels
    t.replace("a","あ"); t.replace("i","い"); t.replace("u","う"); t.replace("e","え"); t.replace("o","お");
    // any remaining n is syllabic (all n+vowel/y mora already converted above)
    t.replace("n","ん");
    t
}

// ── Backend 2: naive token-hash embedding (no dependencies) ──────────────────
//
// Dimension must match the host's `embed_dim` config (default 256). Rhai
// `const` isn't visible inside functions, so the literal 256 is repeated — keep
// them in sync if you change EMBED_DIM.
const EMBED_DIM = 256;

// Hash a normalized token into [0, EMBED_DIM) via a small FNV-style rolling hash.
fn token_bucket(tok) {
    let h = 2166136261;
    for ch in tok.to_chars() {
        h = (h * 16777619 + ch.to_int()) % 1000000007;
    }
    h % 256   // == EMBED_DIM
}

// One naive embedding: token-frequency histogram over EMBED_DIM buckets,
// L2-normalized so cosine similarity ≈ shared-token ratio.
fn naive_embed(text) {
    let v = [];
    v.pad(256, 0.0);   // == EMBED_DIM
    let norm = normalize(text);   // host fn: lowercase, alnum+CJK, space-collapsed
    if norm != "" {
        for tok in norm.split(" ") {
            if tok != "" {
                let b = token_bucket(tok);
                v[b] = v[b] + 1.0;
            }
        }
        let ss = 0.0;
        for x in v { ss += x * x; }
        if ss > 0.0 {
            let inv = 1.0 / ss.sqrt();
            for i in 0..256 { v[i] = v[i] * inv; }
        }
    }
    v
}

// ── Source classification ─────────────────────────────────────────────────────

fn is_video_source(src) {
    ["youtube", "nicovideo", "soundcloud"].contains(src)
}

// ── Title parsing ─────────────────────────────────────────────────────────────

/// Parse a clean music title into #{main, markers}.
/// "Song Name (Live Ver.) [2022 Remaster]" → main="Song Name", markers=["Live Ver.", "2022 Remaster"]
fn parse_clean_title(title) {
    let pat = "\\(([^)]*)\\)|\\[([^\\]]*)\\]|（([^）]*)）|〜([^〜]*)〜|～([^～]*)～";
    let caps = re_captures_all(pat, title);
    let markers = [];
    for cap in caps {
        let content = "";
        for i in 1..cap.len() {
            if cap[i] != "" { content = cap[i]; content.trim(); break; }
        }
        if content != "" { markers.push(content); }
    }
    // NOTE: in Rhai, .trim() is in-place (returns ()), so call it as a statement.
    let raw = re_replace_all(pat, "", title);
    raw.trim();
    let main = re_replace_all("[\\-– ]+$", "", raw);
    main.trim();
    #{ main: main, markers: markers }
}

/// Parse a video platform title (YouTube, NicoVideo) into #{main, blocks}.
fn parse_video_title(title) {
    // 1. 『song』 / 「song」 → unambiguous main title
    let corner_caps = re_captures_all("[『「]([^』」]+)[』」]", title);
    if corner_caps.len() > 0 {
        let main = corner_caps[0][1];
        main.trim();
        return #{ main: main, blocks: [main] };
    }

    // 2. Remove 【...】 blocks
    let stripped = re_replace_all("【[^】]*】", " ", title);
    stripped.trim();

    // 3. Split on the first separator that actually increases block count
    let seps = [" - ", " – ", " / ", "／", " ｜ ", " | ", "〜", "～"];
    let blocks = [stripped];
    for sep in seps {
        let candidate = [];
        for b in blocks {
            for part in b.split(sep) {
                let t = part;
                t.trim();
                if t != "" { candidate.push(t); }
            }
        }
        if candidate.len() > blocks.len() { blocks = candidate; break; }
    }

    // 4. Filter noise
    let noise = ["music video", "mv", "official", "official video", "official audio",
        "official mv", "lyric video", "lyrics", "audio", "visualizer", "animation",
        "short", "teaser", "trailer", "full ver", "full version", "pv", "cm",
        "歌ってみた", "歌い手", "カバー"];
    let non_noise = blocks.filter(|b| !noise.contains(b.to_lower().trim()));

    // 5. Longest surviving block = best guess at main title
    let main = "";
    for b in non_noise { if b.len() > main.len() { main = b; } }

    #{ main: main, blocks: non_noise }
}

/// Extract version/variant markers from a title as a space-separated string of
/// canonical tokens. Empty string = no markers.
fn version_tokens(title) {
    let rules = [
        ["live", "live"], ["ライブ", "live"],
        ["remix", "remix"], ["リミックス", "remix"],
        ["instrumental", "instrumental"], ["inst.", "instrumental"],
        ["インスト", "instrumental"],
        ["acoustic", "acoustic"], ["アコースティック", "acoustic"],
        ["cover", "cover"], ["カバー", "cover"],
        ["medley", "medley"], ["メドレー", "medley"],
        ["tv size", "tv_size"],
        ["short ver", "short_ver"], ["short version", "short_ver"],
        ["cut ver", "short_ver"], ["cut version", "short_ver"],
        ["one chorus", "short_ver"],
        ["game ver", "game_ver"], ["game version", "game_ver"],
        ["off vocal", "instrumental"], ["karaoke", "instrumental"],
    ];

    let found = #{};
    let paren_caps = re_captures_all("(?i)\\(([^)]+)\\)|\\[([^\\]]+)\\]", title);
    for cap in paren_caps {
        let content = if cap[1] != "" { cap[1] } else { cap[2] };
        let cl = content.to_lower();
        for rule in rules { if cl.contains(rule[0]) { found[rule[1]] = true; } }
    }
    let tl = title.to_lower();
    for rule in rules { if tl.contains(rule[0]) { found[rule[1]] = true; } }

    let tokens = found.keys();
    tokens.sort();
    tokens.join(" ")
}

// ── Per-entry helpers ─────────────────────────────────────────────────────────

fn pick_main_title(entry) {
    for sa in entry.sourced_aliases {
        if !is_video_source(sa.source) {
            let p = parse_clean_title(sa.name);
            if p.main != "" { return p.main; }
        }
    }
    for sa in entry.sourced_aliases {
        if is_video_source(sa.source) {
            let p = parse_video_title(sa.name);
            if p.main != "" { return p.main; }
        }
    }
    ""
}

fn pick_markers(entry) {
    for sa in entry.sourced_aliases {
        if !is_video_source(sa.source) {
            let p = parse_clean_title(sa.name);
            if p.markers.len() > 0 { return p.markers; }
        }
    }
    []
}

fn is_mv_entry(entry) {
    // Must have at least one video-platform source.
    if !entry.pairs.any(|p| is_video_source(p.source)) { return false; }
    // Video-only (no authoritative sources at all), OR has video but no ISRC.
    // ISRCs are assigned to audio recordings only — MV recordings never carry one.
    entry.pairs.all(|p| is_video_source(p.source))
        || !entry.pairs.any(|p| p.source == "isrc")
}

// ── Pair-level feature helpers ────────────────────────────────────────────────

// `ctx` is the host context object (unused here); present so the host can call
// this entry point with the uniform (ctx, a, b) convention.
fn main_title_sim(ctx, a, b) {
    let ta = normalize(pick_main_title(a));
    let tb = normalize(pick_main_title(b));
    if ta == "" && tb == "" { return 1.0; }
    if ta == "" || tb == ""  { return 0.0; }
    str_sim(ta, tb)
}

// `ctx` unused (uniform host calling convention — see main_title_sim).
fn markers_conflict(ctx, a, b) {
    let am = pick_markers(a);
    let bm = pick_markers(b);
    if am.len() == 0 || bm.len() == 0 { return false; }
    let an = am.map(|m| normalize(m));
    let bn = bm.map(|m| normalize(m));
    for x in an { for y in bn { if x == y { return false; } } }
    true
}

fn title_exact(a, b) {
    let na = normalize(a.title);
    na != "" && na == normalize(b.title)
}

fn alias_best(a, b) {
    let best = 0.0;
    for x in a.aliases {
        for y in b.aliases {
            let s = jaro_winkler(normalize(x), normalize(y));
            if s > best { best = s; }
        }
    }
    best
}

fn dur_known(a, b)    { a.duration_ms != () && b.duration_ms != () }
fn dur_delta_ms(a, b) {
    if !dur_known(a, b) { return 0; }
    (a.duration_ms - b.duration_ms).abs()
}

fn artist_overlap(a, b) {
    if a.peer_ids.len() == 0 && b.peer_ids.len() == 0 { return 0.0; }
    let inter = a.peer_ids.filter(|x| b.peer_ids.contains(x)).len();
    let union = a.peer_ids.len() + b.peer_ids.len() - inter;
    if union == 0 { return 0.0; }
    inter.to_float() / union.to_float()
}

fn shared_count_authoritative_ids(a, b) {
    let auth = ["musicbrainz", "spotify", "isrc", "discogs"];
    let auth_a = a.pairs.filter(|p| auth.contains(p.source));
    let auth_b = b.pairs.filter(|p| auth.contains(p.source));
    let shared_count = 0;
    for p in auth_a {
        for q in auth_b {
            if p.source == q.source && p.identifier == q.identifier { shared_count += 1; break; }
        }
    }
    shared_count
}

fn same_release(a, b) {
    let a_releases = a.track_positions.map(|p| p.release_id);
    for pos in b.track_positions { if a_releases.contains(pos.release_id) { return true; } }
    false
}

fn same_release_position(a, b) {
    for ap in a.track_positions {
        if ap.track_no == () { continue; }
        for bp in b.track_positions {
            if ap.release_id == bp.release_id
                && ap.disc_no  == bp.disc_no
                && ap.track_no == bp.track_no
            { return true; }
        }
    }
    false
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

fn decide(ctx, a, b) {
    if a.entry_type == "track"         { return decide_track(ctx, a, b); }
    if a.entry_type == "artist"        { return decide_artist(ctx, a, b); }
    if a.entry_type == "release"       { return decide_release(ctx, a, b); }
    if a.entry_type == "release_group" { return decide_rg(ctx, a, b); }
    distinct()
}

// ── Tracks ────────────────────────────────────────────────────────────────────

fn decide_track(ctx, a, b) {
    let MERGE_MIN      = 0.90;
    let RELATE_MIN     = 0.82;
    let TITLE_ONLY_MIN = 0.95;

    if markers_conflict(ctx, a, b) { return distinct(); }

    let t = main_title_sim(ctx, a, b);

    if same_release_position(a, b) {
        if t >= 0.70 { return merge(max(t, 0.90), "same release position"); }
        return relate("alt_version", 0.75, "same release position, titles differ");
    }

    if t < RELATE_MIN { return distinct(); }

    let va = version_tokens(a.title);
    let vb = version_tokens(b.title);
    let version_differs = va != vb;

    // Detect MV pair: exactly one side is an MV entry.
    let a_video = is_mv_entry(a);
    let b_video = is_mv_entry(b);
    let is_mv_pair = (a_video || b_video) && !(a_video && b_video);

    // Duration gate — MV-aware.
    //   Normal:  ±2s.
    //   MV pair: also accept MV longer by up to 1 min (intros/outros), but not shorter.
    let dur_ok = dur_known(a, b) && {
        let delta = dur_delta_ms(a, b);
        if is_mv_pair {
            let mv_dur  = if a_video { a.duration_ms } else { b.duration_ms };
            let ref_dur = if a_video { b.duration_ms } else { a.duration_ms };
            delta <= 2000 || (mv_dur >= ref_dur && mv_dur - ref_dur <= 60000)
        } else {
            delta <= 2000
        }
    };

    if dur_ok && !version_differs && t >= MERGE_MIN {
        return merge(t, if is_mv_pair { "title + MV duration compatible" } else { "title + duration ±2s" });
    }

    let has_corroboration = artist_overlap(a, b) >= 0.5
        || shared_count_authoritative_ids(a, b) > 0
        || same_release(a, b);

    // MV with a cut/short token: duration is intentionally different; merge on
    // title + corroboration alone since it's the same song in a shorter promotional edit.
    let has_cut_token = va.contains("short_ver") || vb.contains("short_ver");
    if is_mv_pair && has_cut_token && has_corroboration && t >= MERGE_MIN {
        return merge(t, "MV short/cut version, same song");
    }

    if has_corroboration {
        let kind = classify_version(va, vb);
        let reason = if dur_known(a, b) {
            "title match, duration differs or version marked"
        } else {
            "title match, no duration available"
        };
        return relate(kind, t * 0.9, reason);
    }

    if !version_differs && t >= TITLE_ONLY_MIN {
        return relate("alt_version", t * 0.75, "title match only, no corroboration");
    }

    distinct()
}

fn classify_version(va, vb) {
    let all = va + " " + vb;
    if all.contains("live")         { return "live"; }
    if all.contains("remix")        { return "remix"; }
    if all.contains("instrumental") { return "instrumental"; }
    if all.contains("acoustic")     { return "acoustic"; }
    if all.contains("cover")        { return "cover"; }
    if all.contains("medley")       { return "medley"; }
    "alt_version"
}

// ── Artists ───────────────────────────────────────────────────────────────────

fn decide_artist(ctx, a, b) {
    let MERGE_MIN  = 0.90;
    let RELATE_MIN = 0.80;

    let sim = max(alias_best(a, b), if title_exact(a, b) { 1.0 } else { jaccard(normalize(a.title), normalize(b.title)) });
    if sim < RELATE_MIN { return distinct(); }

    let has_corroboration = artist_overlap(a, b) > 0.0 || shared_count_authoritative_ids(a, b) > 0;
    if sim >= MERGE_MIN && has_corroboration {
        return merge(sim, "alias match + shared_count credits or authoritative ID");
    }

    relate("same_artist", sim * 0.75, "alias match, no shared_count credits")
}

// ── Releases ──────────────────────────────────────────────────────────────────

fn decide_release(ctx, a, b) {
    let RELATE_MIN = 0.82;

    let t = main_title_sim(ctx, a, b);
    if t < RELATE_MIN { return distinct(); }
    relate("release_variant", t, "title match — likely release variant")
}

// ── Release groups ────────────────────────────────────────────────────────────

fn decide_rg(ctx, a, b) {
    let MERGE_MIN = 0.85;

    let t = main_title_sim(ctx, a, b);
    if t < MERGE_MIN { return distinct(); }
    if artist_overlap(a, b) >= 0.5 || title_exact(a, b) {
        return merge(t, "title + artist overlap");
    }
    distinct()
}
