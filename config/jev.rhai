// jev.rhai — TypeSafe Jev as a soft-dedup refine step (docs/plan-jev-in-script.md).
//
// A module: match scripts `import "jev" as jev;` inside the functions that use
// it (imports at the top level aren't visible to hooks the host calls).
// This file owns the evidence view and the answer → verdict mapping; the
// questions are config/jev/<entry type>.json, sent verbatim (key order kept).
// The transport (HTTP, retry, concurrency, response cache) is the inference
// cdylib's `inference_typesafe_*` API (feature `typesafe`).
//
// Questions are prompt v2.1 from train/learned-matcher/jev_client.py, aligned
// with the learned matcher's ontology. Tracks: identity (same_identity /
// derived / sibling / unrelated) plus kind and direction for derived pairs.
// Other types: identity only (same_identity / different_identity).
//
//   let j = jev::open(lib);              // () when disabled (logged why)
//   let verdicts = jev::refine_pairs(j, items);   // items as refine() gets them
//   jev::close(j);
//
// Enabled when TYPESAFE_API_KEY is set and libinference.so has the `typesafe`
// feature. Every answer is cached in <cache_dir>/typesafe.db keyed by the
// request, so asking again about unchanged evidence is free; any edit to the
// questions or the view below is a new request. Cost is per input token
// (~1.9k tokens for a track request, ~1.1k for the others).

fn model() { "jev-latest" }
// Recorded on every row a Jev verdict writes (dedup_feedback/entry_relation).
fn model_version() { "typesafe-jev/v2.1" }
// Concurrent API calls per batch; MUSICLIB_JEV_CONCURRENCY overrides. The
// cache path is MUSICLIB_JEV_CACHE, else <cache_dir>/typesafe.db.
fn default_concurrency() { 12 }

// Open the TypeSafe client through `lib` (an ffi library, or () to look for
// libinference.so). Returns the client map, or () when Jev is unavailable.
fn open(lib) {
    let key = env_var("TYPESAFE_API_KEY");
    if key == "" {
        print("jev: TYPESAFE_API_KEY not set; DEFER verdicts stay deferred");
        return ();
    }
    try {
        if type_of(lib) == "()" { lib = open_library(); }
        let j = #{
            lib: lib,
            open:       lib.func("inference_typesafe_open", "ptr", ["ptr"]),
            close:      lib.func("inference_typesafe_close", "void", ["ptr"]),
            ask_batch:  lib.func("inference_typesafe_ask_batch", "ptr", ["ptr", "ptr"]),
            last_error: lib.func("inference_typesafe_last_error", "ptr", []),
            free_str:   lib.func("inference_free_string", "void", ["ptr"]),
        };
        let concurrency = default_concurrency();
        let env = env_var("MUSICLIB_JEV_CONCURRENCY");
        if env != "" { concurrency = parse_int(env); }
        let cache_path = env_var("MUSICLIB_JEV_CACHE");
        if cache_path == "" { cache_path = cache_dir() + "/typesafe.db"; }
        let config = #{ api_key: key, cache_path: cache_path, concurrency: concurrency };
        let base_url = env_var("TYPESAFE_BASE_URL");   // e.g. a local mock
        if base_url != "" { config.base_url = base_url; }
        let cs = ffi::cstr(to_json(config));
        let h = j.open.invoke([cs]);
        ffi::free(cs);
        if h.is_null() { throw "inference_typesafe_open: " + ffi::read_cstr(j.last_error.invoke([])); }
        j.h = h;
        print("jev: enabled (" + model() + ", " + concurrency + " concurrent, cache " + cache_path + ")");
        return j;
    } catch (err) {
        print("jev: unavailable, DEFER verdicts stay deferred: " + err);
    }
    ()
}

fn close(j) {
    if type_of(j) == "map" { j.close.invoke([j.h]); }
}

fn open_library() {
    if !ffi_available() { throw "musiclib built without --features ffi"; }
    let candidates = ["./libinference.so", "../target/release/libinference.so", "libinference.so"];
    for path in candidates {
        try { return ffi::open(path); } catch (err) {}
    }
    throw "libinference.so not found (script dir, ../target/release, system path)";
}

// One verdict (or () to keep the pair's own) per item, items being
// `#{a, b, verdict}` as the host passes them to refine(). Pairs of different
// types are never asked. Logs one summary line per call.
fn refine_pairs(j, items) {
    let out = items.map(|it| ());
    // Read here, not kept in `j`: the host copies the script context on
    // every decide() call.
    let qs = questions();
    let asked = [];
    let requests = [];
    for i in 0..items.len() {
        let it = items[i];
        if it.a.entry_type != it.b.entry_type || !(it.a.entry_type in qs) { continue; }
        asked.push(i);
        requests.push(request(qs, it.a, it.b));
    }
    if requests.is_empty() { return out; }

    let cs = ffi::cstr("[" + join(requests, ",") + "]");
    let p = j.ask_batch.invoke([j.h, cs]);
    ffi::free(cs);
    if p.is_null() { throw "inference_typesafe_ask_batch: " + ffi::read_cstr(j.last_error.invoke([])); }
    let results = parse_json(ffi::read_cstr(p));
    j.free_str.invoke([p]);

    let counts = #{ cached: 0, asked: 0, failed: 0, merge: 0, relate: 0, distinct: 0, defer: 0 };
    let first_error = "";
    for k in 0..asked.len() {
        let r = results[k];
        if "error" in r {
            counts.failed += 1;
            if first_error == "" { first_error = r.error; }
            continue;
        }
        if r.cached { counts.cached += 1; } else { counts.asked += 1; }
        let v = to_verdict(r.ok, items[asked[k]].a.entry_type);
        if type_of(v) == "()" {
            counts.failed += 1;
            if first_error == "" { first_error = "response has no identity answer"; }
            continue;
        }
        counts[v.verdict] += 1;
        out[asked[k]] = v;
    }
    let line = "jev: " + asked.len() + " pair(s): " + counts.cached + " cached, "
        + counts.asked + " asked, " + counts.failed + " failed -> "
        + counts.merge + " merge, " + counts.relate + " relate, "
        + counts.distinct + " distinct, " + counts.defer + " still deferred";
    if first_error != "" { line += " (first error: " + first_error + ")"; }
    print(line);
    out
}

// The questions per entry type, as JSON text (config/jev/<type>.json).
fn questions() {
    let qs = #{};
    for t in ["track", "artist", "release", "release_group"] {
        qs[t] = read_text("jev/" + t + ".json");
    }
    qs
}

// One request as JSON text, keys in the order the prompts were validated with.
fn request(qs, a, b) {
    obj([
        ["state", raw(obj([["a", raw(view_json(a))], ["b", raw(view_json(b))]]))],
        ["model", model()],
        ["questions", raw(qs[a.entry_type])],
    ])
}

// Jev's answer as a soft verdict, () when it has no identity answer:
//   same_identity                        → MERGE (a reversible same_identity assertion)
//   derived (tracks)                     → RELATE derived_from, with the kind as
//                                          `transformation` and the direction as `derived_side`
//   sibling / unrelated / different_identity → DISTINCT (siblings get no direct edge)
//   unsure                               → DEFER
fn to_verdict(resp, entry_type) {
    let answers = resp.answers;
    if type_of(answers) != "map" || !("identity" in answers) { return (); }
    let id = answers.identity;
    if id["type"] != "choice" { return (); }
    let conf = id.confidence;
    let choice = id.choice;
    let kind = choice_of(answers, "kind");
    let direction = choice_of(answers, "direction");
    let reason = model() + " v2.1: " + choice;
    if entry_type == "track" { reason += " (kind=" + kind + ", direction=" + direction + ")"; }
    let v = if choice == "same_identity" {
        merge(conf, reason)
    } else if choice == "derived" {
        let meta = #{};
        // The learned matcher's kinds; alt_version has no counterpart.
        if ["edit", "live", "remix", "instrumental", "cover", "arrangement"].contains(kind) {
            meta.transformation = kind;
        }
        if direction == "a_is_original" { meta.derived_side = "b"; }
        if direction == "b_is_original" { meta.derived_side = "a"; }
        relate("derived_from", conf, reason, meta)
    } else if ["sibling", "unrelated", "different_identity"].contains(choice) {
        distinct()
    } else {
        defer(conf, reason)
    };
    v.origin = "jev";
    v.model_version = model_version();
    v
}

fn choice_of(answers, name) {
    if name in answers && answers[name]["type"] == "choice" { answers[name].choice } else { "?" }
}

// ── Ordered JSON ─────────────────────────────────────────────────────────────
//
// Rhai maps sort their keys, so requests are built as JSON text: `obj` takes
// [key, value] pairs in order; `raw(text)` marks a value that is JSON already.

fn raw(text) { #{ raw_json: text } }

fn enc(v) {
    if type_of(v) == "map" && "raw_json" in v { v.raw_json } else { to_json(v) }
}

fn obj(fields) {
    let parts = [];
    for f in fields { parts.push(to_json(f[0]) + ":" + enc(f[1])); }
    "{" + join(parts, ",") + "}"
}

fn arr(items) {
    let parts = [];
    for x in items { parts.push(enc(x)); }
    "[" + join(parts, ",") + "]"
}

// ── Evidence view ────────────────────────────────────────────────────────────
//
// What Jev sees of one entry: its own fields plus names resolved from the
// pair facts (credited artists/tracks, release titles, child tracks). Lists
// are capped so a large release or a prolific artist stays a small request.

fn view(entry) {
    let facts = pair_facts_json(entry.pairs);
    facts = if type_of(facts) == "string" { parse_json(facts) } else { [] };
    // Clean sources first, so their names win where several pairs name the
    // same related entry.
    facts.sort(|x, y| source_rank(x.source) - source_rank(y.source));

    let titles = entry.sourced_aliases.extract(0, 8).map(|s| #{ source: s.source, name: s.name });

    let sources = entry.pairs.map(|p| p.source);
    sources.sort();
    sources.dedup();

    let handles = [];
    for p in entry.pairs {
        let h = extract_handle(p.identifier);
        if type_of(h) == "string" { handles.push(h); }
    }
    handles.sort();
    handles.dedup();
    handles.truncate(6);

    // Names keyed by entry, first (cleanest) name wins: credited artists for
    // tracks/releases, credited tracks for artists, child tracks for releases.
    let credited = #{};
    let children = #{};
    let positions = #{};
    let release_group_ids = [];
    for f in facts {
        for c in f.contributions {
            credited.add_credit(c.artist_entry_id, c.artist_name);
        }
        let names = if "credited_names" in f { f.credited_names } else { [] };
        for i in 0..f.credited.len() {
            credited.add_credit(f.credited[i], if i < names.len() { names[i] } else { () });
        }
        for p in f.parents {
            if p.entry_type == "release" {
                // Keyed by what Jev sees: editions of one album collapse.
                let key = to_json([p.name, p.disc, p.track]);
                if !(key in positions) {
                    positions[key] = #{ id: p.entry_id, release_title: p.name, disc_no: p.disc, track_no: p.track };
                }
            } else if p.entry_type == "release_group" && entry.entry_type == "release"
                && type_of(p.entry_id) == "i64" {
                release_group_ids.push(p.entry_id);
            }
        }
        for c in f.children {
            if c.entry_type == "track" { children.add_credit(c.entry_id, c.name); }
        }
    }
    let credited_names = credited.values();
    credited_names.sort();
    credited_names.dedup();
    credited_names.truncate(8);
    let release_tracks = children.values();
    release_tracks.sort();
    release_tracks.dedup();
    release_tracks.truncate(12);
    release_group_ids.sort();
    release_group_ids.dedup();
    let track_positions = positions.values();
    track_positions.sort(|x, y| sort_key(x) - sort_key(y));
    track_positions = track_positions.extract(0, 5).map(|p| #{
        release_title: p.release_title, disc_no: p.disc_no, track_no: p.track_no,
    });

    #{
        entry_type: entry.entry_type,
        titles: titles,
        durations_sec: entry.durations.map(|ms| ms.to_float() / 1000.0),
        release_dates: entry.release_dates,
        release_types: entry.release_types,
        primary_types: entry.primary_types,
        sources: sources,
        credited_names: credited_names,
        track_positions: track_positions,
        release_tracks: release_tracks,
        handles: handles,
        release_group_ids: release_group_ids,
    }
}

// `view(entry)` as JSON text, in the field order of the original Rust view
// (and the training client's), which the prompts were validated against.
fn view_json(entry) {
    let v = view(entry);
    let titles = [];
    for t in v.titles { titles.push(raw(obj([["source", t.source], ["name", t.name]]))); }
    let positions = [];
    for p in v.track_positions {
        positions.push(raw(obj([
            ["release_title", p.release_title], ["disc_no", p.disc_no], ["track_no", p.track_no],
        ])));
    }
    obj([
        ["entry_type", v.entry_type],
        ["titles", raw(arr(titles))],
        ["durations_sec", v.durations_sec],
        ["release_dates", v.release_dates],
        ["release_types", v.release_types],
        ["primary_types", v.primary_types],
        ["sources", v.sources],
        ["credited_names", v.credited_names],
        ["track_positions", raw(arr(positions))],
        ["release_tracks", v.release_tracks],
        ["handles", v.handles],
        ["release_group_ids", v.release_group_ids],
    ])
}

fn source_rank(source) {
    if ["youtube", "nicovideo", "soundcloud", "bilibili"].contains(source) { 1 } else { 0 }
}

// Called as `map.add_credit(id, name)` (a method, so it can update the map):
// the first name seen per entry, unresolved entries keyed by name.
fn add_credit(id, name) {
    if type_of(name) != "string" || name == "" { return; }
    let key = if type_of(id) == "i64" { id.to_string() } else { "name:" + name };
    if !(key in this) { this[key] = name; }
}

// Release id, then disc, then track; () sorts first.
fn sort_key(p) {
    (opt_int(p.id) * 1000 + opt_int(p.disc_no)) * 10000 + opt_int(p.track_no)
}

fn opt_int(x) { if type_of(x) == "i64" { x + 1 } else { 0 } }

// A human-chosen name from a source URL's last path segment (a channel
// handle, an artist slug), or () when it looks like an opaque platform id.
fn extract_handle(identifier) {
    let at = identifier.index_of("://");
    if at < 0 { return (); }
    let rest = identifier.sub_string(at + 3);
    for sep in ["?", "#"] {
        let k = rest.index_of(sep);
        if k >= 0 { rest = rest.sub_string(0, k); }
    }
    let segments = rest.split("/").filter(|s| s != "");
    if segments.is_empty() { return (); }
    let core = segments[-1];
    // YouTube video ids (youtu.be/ID, /shorts/ID, …; /watch?v=ID ends in "watch").
    if rest.starts_with("youtu.be/") || core == "watch" { return (); }
    if segments.len() >= 2 && rest.contains("youtube.com")
        && ["shorts", "embed", "live"].contains(segments[-2]) { return (); }
    while core.starts_with("@") { core = core.sub_string(1); }
    if core == "" { return (); }
    if re_is_match("^(id)?[0-9]+$", core) { return (); }                  // numeric catalog id
    if re_is_match("^UC[A-Za-z0-9_-]{22}$", core) { return (); }          // youtube channel id
    if core.len() == 36 && core.split("-").len() == 5 { return (); }     // musicbrainz uuid
    if re_is_match("^[0-9a-fA-F]{20,}$", core) { return (); }             // hex hash
    if re_is_match("^[A-Z][0-9]{4,}$", core) { return (); }               // ASIN, wikidata Q-id, …
    if core.len() >= 15 && re_is_match("[0-9]", core) && re_is_match("[A-Z]", core)
        && re_is_match("[a-z]", core) { return (); }                      // base62 opaque id
    let decoded = url_decode(core);
    if decoded == "" { () } else { decoded }
}
