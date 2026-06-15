// match.rhai — heuristic soft-dedup policy.
// Copy to <config_dir>/match.rhai and tune thresholds as needed.
//
// Lifecycle:  init()         called once after the script is loaded (optional)
//             decide(a, b)   called for each candidate pair
//             destroy()      called when the engine shuts down (optional)
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
// NOTE: Rhai functions have their own scope — define thresholds inside each
// function (not at the top level) so they are visible where they are used.

// ── Lifecycle ─────────────────────────────────────────────────────────────────
//
// Patterns used repeatedly are best pre-compiled in init() so each decide()
// call gets a cheap Arc clone rather than a cache lookup:
//
//   fn init() {
//       global::CLEAN_PAT = compile_re("\\(([^)]*)\\)|\\[([^\\]]*)\\]|…");
//       global::PAREN_PAT = compile_re("(?i)\\(([^)]+)\\)|\\[([^\\]]+)\\]");
//   }
//
//   fn parse_clean_title(title) {
//       let caps = re_captures_all(global::CLEAN_PAT, title);
//       …
//   }
//
// Pattern strings still work everywhere (compiled on first use, cached per
// engine), so pre-compilation is optional but saves one Mutex acquire per call.

// ── Source classification ─────────────────────────────────────────────────────

let VIDEO_SOURCES        = ["youtube", "nicovideo", "soundcloud"];
let AUTHORITATIVE_SOURCES = ["musicbrainz", "spotify", "isrc", "discogs"];

fn is_video_source(src) { VIDEO_SOURCES.contains(src) }

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
            if cap[i] != "" { content = cap[i].trim(); break; }
        }
        if content != "" { markers.push(content); }
    }
    let raw = re_replace_all(pat, "", title).trim();
    let main = re_replace_all("[\\-– ]+$", "", raw).trim();
    #{ main: main, markers: markers }
}

/// Parse a video platform title (YouTube, NicoVideo) into #{main, blocks}.
fn parse_video_title(title) {
    // 1. 『song』 / 「song」 → unambiguous main title
    let corner_caps = re_captures_all("[『「]([^』」]+)[』」]", title);
    if corner_caps.len() > 0 {
        let main = corner_caps[0][1].trim();
        return #{ main: main, blocks: [main] };
    }

    // 2. Remove 【...】 blocks
    let stripped = re_replace_all("【[^】]*】", " ", title).trim();

    // 3. Split on the first separator that actually increases block count
    let seps = [" - ", " – ", " / ", "／", " ｜ ", " | ", "〜", "～"];
    let blocks = [stripped];
    for sep in seps {
        let candidate = [];
        for b in blocks {
            for part in b.split(sep) {
                let t = part.trim();
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

// ── Pair-level feature helpers ────────────────────────────────────────────────

fn main_title_sim(a, b) {
    let ta = normalize(pick_main_title(a));
    let tb = normalize(pick_main_title(b));
    if ta == "" && tb == "" { return 1.0; }
    if ta == "" || tb == ""  { return 0.0; }
    str_sim(ta, tb)
}

fn markers_conflict(a, b) {
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
    let auth_a = a.pairs.filter(|p| AUTHORITATIVE_SOURCES.contains(p.source));
    let auth_b = b.pairs.filter(|p| AUTHORITATIVE_SOURCES.contains(p.source));
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

fn decide(a, b) {
    if a.entry_type == "track"         { return decide_track(a, b); }
    if a.entry_type == "artist"        { return decide_artist(a, b); }
    if a.entry_type == "release"       { return decide_release(a, b); }
    if a.entry_type == "release_group" { return decide_rg(a, b); }
    distinct()
}

// ── Tracks ────────────────────────────────────────────────────────────────────

fn decide_track(a, b) {
    let MERGE_MIN      = 0.90;
    let RELATE_MIN     = 0.82;
    let TITLE_ONLY_MIN = 0.95;

    if markers_conflict(a, b) { return distinct(); }

    let t = main_title_sim(a, b);

    if same_release_position(a, b) {
        if t >= 0.70 { return merge(max(t, 0.90), "same release position"); }
        return relate("alt_version", 0.75, "same release position, titles differ");
    }

    if t < RELATE_MIN { return distinct(); }

    let va = version_tokens(a.title);
    let vb = version_tokens(b.title);
    let version_differs = va != vb;

    if dur_known(a, b) && dur_delta_ms(a, b) <= 2000 && !version_differs {
        if t >= MERGE_MIN { return merge(t, "title + duration ±2s"); }
    }

    let has_corroboration = artist_overlap(a, b) >= 0.5
        || shared_count_authoritative_ids(a, b) > 0
        || same_release(a, b);

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

fn decide_artist(a, b) {
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

fn decide_release(a, b) {
    let RELATE_MIN = 0.82;

    let t = main_title_sim(a, b);
    if t < RELATE_MIN { return distinct(); }
    relate("release_variant", t, "title match — likely release variant")
}

// ── Release groups ────────────────────────────────────────────────────────────

fn decide_rg(a, b) {
    let MERGE_MIN = 0.85;

    let t = main_title_sim(a, b);
    if t < MERGE_MIN { return distinct(); }
    if artist_overlap(a, b) >= 0.5 || title_exact(a, b) {
        return merge(t, "title + artist overlap");
    }
    distinct()
}
